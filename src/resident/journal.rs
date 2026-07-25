use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::{EventSequence, RequestId};

/// Persistence strength applied to journal appends.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Durability {
    /// Write to the operating-system page cache.
    #[default]
    Flush,
    /// Synchronize data before acknowledging the mutation.
    Fsync,
}

/// One checksummed semantic event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalRecord<E> {
    /// Session event sequence.
    pub sequence: EventSequence,
    /// Request producing this event, if any.
    pub request: Option<RequestId>,
    /// Domain event.
    pub event: E,
}

#[derive(Deserialize)]
struct RawStoredRecord {
    record: Box<RawValue>,
    checksum: u32,
}

#[derive(Serialize)]
struct StoredRecordRef<'a, E> {
    record: &'a JournalRecord<E>,
    checksum: u32,
}

/// Journal failures.
#[derive(Debug)]
pub enum JournalError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// JSON encoding or decoding failed.
    Json(serde_json::Error),
    /// A complete record failed its checksum.
    Corrupt {
        /// One-based record line.
        line: usize,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Corrupt { line } => write!(formatter, "journal checksum failed at line {line}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Append-only JSON journal with checksummed records.
pub struct FileJournal<E> {
    path: PathBuf,
    durability: Durability,
    marker: PhantomData<fn() -> E>,
}

pub(crate) struct JournalWriter<E> {
    file: File,
    durability: Durability,
    marker: PhantomData<fn() -> E>,
}

impl<E> JournalWriter<E>
where
    E: Serialize,
{
    pub(crate) fn append(&mut self, record: &JournalRecord<E>) -> Result<(), JournalError> {
        write_record(&mut self.file, record)?;
        self.file.flush()?;
        if self.durability == Durability::Fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

impl<E> FileJournal<E>
where
    E: DeserializeOwned + Serialize,
{
    /// Opens a journal at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, durability: Durability) -> Self {
        Self {
            path: path.into(),
            durability,
            marker: PhantomData,
        }
    }

    /// Appends one complete record.
    pub fn append(&self, record: &JournalRecord<E>) -> Result<(), JournalError> {
        self.writer()?.append(record)
    }

    pub(crate) fn writer(&self) -> Result<JournalWriter<E>, JournalError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        reject_symlink(&self.path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        truncate_torn_tail(&mut file)?;
        Ok(JournalWriter {
            file,
            durability: self.durability,
            marker: PhantomData,
        })
    }

    /// Loads all complete records. A final torn line is ignored.
    pub fn load(&self) -> Result<Vec<JournalRecord<E>>, JournalError> {
        reject_symlink(&self.path)?;
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut line_number = 0;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            line_number += 1;
            if !line.ends_with(b"\n") {
                break;
            }
            line.pop();
            let stored: RawStoredRecord = serde_json::from_slice(&line)?;
            if crc32fast::hash(stored.record.get().as_bytes()) != stored.checksum {
                return Err(JournalError::Corrupt { line: line_number });
            }
            records.push(serde_json::from_str(stored.record.get())?);
        }
        Ok(records)
    }

    /// Atomically replaces the journal with a compacted record set.
    pub fn compact(&self, records: &[JournalRecord<E>]) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        reject_symlink(&self.path)?;
        let temporary = self
            .path
            .with_extension(format!("journal.tmp-{}", std::process::id()));
        reject_symlink(&temporary)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        for record in records {
            let payload = serde_json::to_vec(record)?;
            serde_json::to_writer(
                &mut file,
                &StoredRecordRef {
                    record,
                    checksum: crc32fast::hash(&payload),
                },
            )?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        fs::rename(temporary, &self.path)?;
        Ok(())
    }

    /// Atomically writes a JSON checkpoint.
    pub fn write_checkpoint<T: Serialize>(path: &Path, value: &T) -> Result<(), JournalError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        reject_symlink(path)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        reject_symlink(&temporary)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    /// Reads an optional JSON checkpoint.
    pub fn read_checkpoint<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, JournalError> {
        reject_symlink(path)?;
        match File::open(path) {
            Ok(file) => serde_json::from_reader(file).map(Some).map_err(Into::into),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn write_record<E: Serialize>(
    file: &mut File,
    record: &JournalRecord<E>,
) -> Result<(), JournalError> {
    let payload = serde_json::to_vec(record)?;
    serde_json::to_writer(
        &mut *file,
        &StoredRecordRef {
            record,
            checksum: crc32fast::hash(&payload),
        },
    )?;
    file.write_all(b"\n")?;
    Ok(())
}

fn truncate_torn_tail(file: &mut File) -> io::Result<()> {
    const SCAN_BYTES: u64 = 8 * 1024;

    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    let mut end = length;
    let mut buffer = vec![0_u8; SCAN_BYTES as usize];
    while end > 0 {
        let start = end.saturating_sub(SCAN_BYTES);
        let read = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..read])?;
        if let Some(index) = buffer[..read].iter().rposition(|byte| *byte == b'\n') {
            file.set_len(start + index as u64 + 1)?;
            return Ok(());
        }
        end = start;
    }
    file.set_len(0)
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing symlinked resident storage file: {}",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct LegacyEvent {
        value: String,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct CurrentEvent {
        value: String,
        #[serde(default)]
        added_later: Option<u64>,
    }

    fn path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("turtletap-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn ignores_a_torn_final_record() {
        let path = path("torn-journal");
        let journal = FileJournal::new(&path, Durability::Flush);
        let record = JournalRecord {
            sequence: EventSequence(1),
            request: None,
            event: "committed".to_owned(),
        };
        journal.append(&record).expect("append");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open")
            .write_all(b"{\"record\":")
            .expect("tear");
        assert_eq!(journal.load().expect("load"), vec![record]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn append_truncates_a_torn_final_record() {
        let path = path("append-after-torn-journal");
        let journal = FileJournal::new(&path, Durability::Flush);
        let first = JournalRecord {
            sequence: EventSequence(1),
            request: None,
            event: "first".to_owned(),
        };
        let second = JournalRecord {
            sequence: EventSequence(2),
            request: None,
            event: "second".to_owned(),
        };
        journal.append(&first).expect("append first");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open")
            .write_all(b"{\"record\":")
            .expect("tear");

        journal.append(&second).expect("append after torn tail");

        assert_eq!(journal.load().expect("load"), vec![first, second]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn compaction_preserves_the_selected_records() {
        let path = path("compact-journal");
        let journal = FileJournal::new(&path, Durability::Flush);
        let first = JournalRecord {
            sequence: EventSequence(1),
            request: None,
            event: "first".to_owned(),
        };
        let latest = JournalRecord {
            sequence: EventSequence(2),
            request: None,
            event: "latest".to_owned(),
        };
        journal.append(&first).expect("append");
        journal.append(&latest).expect("append");
        journal
            .compact(std::slice::from_ref(&latest))
            .expect("compact");
        assert_eq!(journal.load().expect("load"), vec![latest]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn checksum_survives_compatible_schema_evolution() {
        let path = path("schema-evolution-journal");
        let legacy = FileJournal::new(&path, Durability::Flush);
        legacy
            .append(&JournalRecord {
                sequence: EventSequence(1),
                request: None,
                event: LegacyEvent {
                    value: "before-upgrade".to_owned(),
                },
            })
            .expect("append legacy record");

        let current = FileJournal::<CurrentEvent>::new(&path, Durability::Flush);
        assert_eq!(
            current.load().expect("load evolved record"),
            vec![JournalRecord {
                sequence: EventSequence(1),
                request: None,
                event: CurrentEvent {
                    value: "before-upgrade".to_owned(),
                    added_later: None,
                },
            }]
        );
        let _ = fs::remove_file(path);
    }
}
