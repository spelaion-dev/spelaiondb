//! Moving a local store that cannot be opened out of the way, so the app can start with a new
//! one (SDB-283). The file is renamed beside itself, never deleted: its history, saved queries
//! and append-only audit log stay readable in it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::StoreError;

/// The files SQLite may keep beside a database. They belong to it and move with it: a write-ahead
/// log left behind would otherwise meet the new store of the same name.
const COMPANIONS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Where [`set_aside`] moves the store at `path` on the day of `now` (UTC):
/// `state.db.broken-2026-11-16`, then `…-2`, `…-3` when a name is already taken.
pub fn aside_path(path: &Path, now: SystemTime) -> PathBuf {
    let base = format!("{}.broken-{}", path.display(), utc_date(now));
    let mut candidate = PathBuf::from(&base);
    let mut n = 2;
    while taken(&candidate) {
        candidate = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// Moves the store at `path`, and the files SQLite keeps beside it, to [`aside_path`]. Returns
/// where it went. Nothing is overwritten and nothing is deleted: if one file cannot be moved,
/// the ones already moved are put back and the error says which.
pub fn set_aside(path: &Path, now: SystemTime) -> Result<PathBuf, StoreError> {
    let target = aside_path(path, now);
    let mut moves = vec![(path.to_path_buf(), target.clone())];
    for suffix in COMPANIONS {
        let from = with_suffix(path, suffix);
        if from.exists() {
            moves.push((from, with_suffix(&target, suffix)));
        }
    }
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for pair in &moves {
        let (from, to) = pair;
        if let Err(e) = std::fs::rename(from, to) {
            for (back_from, back_to) in done.iter().rev() {
                let _ = std::fs::rename(back_to, back_from);
            }
            return Err(StoreError::new(format!(
                "could not move {} to {}: {e}",
                from.display(),
                to.display()
            )));
        }
        done.push(pair);
    }
    Ok(target)
}

/// A name is taken when the file or any of its companions exists.
fn taken(candidate: &Path) -> bool {
    candidate.exists()
        || COMPANIONS
            .iter()
            .any(|suffix| with_suffix(candidate, suffix).exists())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `YYYY-MM-DD` of `now` in UTC.
fn utc_date(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// The date of a day counted from 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2026-11-16 12:00 UTC.
    fn day() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_794_830_400)
    }

    /// An empty folder of this test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-aside-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dates_are_utc_days() {
        assert_eq!(utc_date(UNIX_EPOCH), "1970-01-01");
        assert_eq!(utc_date(day()), "2026-11-16");
        assert_eq!(
            utc_date(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29"
        );
    }

    #[test]
    fn the_store_and_its_companions_move_together_and_nothing_is_overwritten() {
        let dir = scratch("moves");
        let db = dir.join("state.db");
        std::fs::write(&db, b"old store").unwrap();
        std::fs::write(with_suffix(&db, "-wal"), b"old log").unwrap();
        // An earlier move the same day keeps its name.
        let earlier = dir.join("state.db.broken-2026-11-16");
        std::fs::write(&earlier, b"earlier").unwrap();

        let moved = set_aside(&db, day()).unwrap();
        assert_eq!(moved, dir.join("state.db.broken-2026-11-16-2"));
        assert!(!db.exists());
        assert!(!with_suffix(&db, "-wal").exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"old store");
        assert_eq!(
            std::fs::read(with_suffix(&moved, "-wal")).unwrap(),
            b"old log"
        );
        assert_eq!(std::fs::read(&earlier).unwrap(), b"earlier");
    }

    #[test]
    fn a_name_whose_companion_is_left_is_taken() {
        let dir = scratch("taken");
        let db = dir.join("state.db");
        std::fs::write(&db, b"store").unwrap();
        std::fs::write(dir.join("state.db.broken-2026-11-16-shm"), b"x").unwrap();
        assert_eq!(
            aside_path(&db, day()),
            dir.join("state.db.broken-2026-11-16-2")
        );
    }

    #[test]
    fn a_missing_store_is_an_error_and_moves_nothing() {
        let dir = scratch("missing");
        let db = dir.join("state.db");
        let error = set_aside(&db, day()).unwrap_err().to_string();
        assert!(error.contains("could not move"), "{error}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
