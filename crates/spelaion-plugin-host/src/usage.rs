//! What a plugin's process uses (SDB-399): its memory and the processor time it has taken,
//! read from the operating system for that one process, with no privilege beyond what the
//! app already has over the child it started, and no `unsafe` (the workspace forbids it):
//!
//! - Windows: the working set (`GetProcessMemoryInfo`) and the processor time of a job object
//!   the process is put in when it starts, both through `win32job`'s safe calls;
//! - Linux: `/proc/<pid>/statm` and `/proc/<pid>/stat`, with the page size and the clock's
//!   ticks from `rustix`;
//! - macOS and the other Unix systems: `ps -o rss=,time= -p <pid>`.
//!
//! A number that cannot be read is `None`, never a guess.

use std::process::Child;
use std::time::{Duration, Instant};

/// Reads one process's numbers; made when it starts.
pub(crate) struct Meter {
    pid: u32,
    started: Instant,
    /// The job the process is in, for its processor time (Windows).
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

/// One reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reading {
    pub memory_bytes: Option<u64>,
    pub cpu: Option<Duration>,
}

impl Meter {
    pub(crate) fn new(child: &Child) -> Self {
        Self {
            pid: child.id(),
            started: Instant::now(),
            #[cfg(windows)]
            job: windows::job_for(child),
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Reads the process's numbers. `child` must still be held by its owner, so the process
    /// (and on Windows its handle) is the one this meter was made for.
    pub(crate) fn read(&self, child: &Child) -> Reading {
        #[cfg(windows)]
        {
            windows::read(child, self.job.as_ref())
        }
        #[cfg(target_os = "linux")]
        {
            let _ = child;
            linux::read(self.pid)
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let _ = child;
            ps::read(self.pid)
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::Reading;
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    /// A job of its own for the process, to read its processor time. Nothing is limited and
    /// closing the job does not end the process.
    pub(super) fn job_for(child: &Child) -> Option<win32job::Job> {
        let job = win32job::Job::create().ok()?;
        job.assign_process(child.as_raw_handle() as isize).ok()?;
        Some(job)
    }

    pub(super) fn read(child: &Child, job: Option<&win32job::Job>) -> Reading {
        let memory = win32job::utils::get_process_memory_info(child.as_raw_handle() as isize)
            .ok()
            .map(|counters| counters.working_set_size as u64);
        let cpu = job
            .and_then(|job| job.query_basic_and_io_accounting_info().ok())
            .map(|info| info.total_user_time + info.total_kernel_time);
        Reading {
            memory_bytes: memory,
            cpu,
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Reading;
    use std::time::Duration;

    pub(super) fn read(pid: u32) -> Reading {
        let memory = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|text| resident_pages(&text))
            .map(|pages| pages * rustix::param::page_size() as u64);
        let cpu = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| cpu_ticks(&text))
            .and_then(|ticks| {
                let per_second = rustix::param::clock_ticks_per_second();
                (per_second > 0).then(|| Duration::from_secs_f64(ticks as f64 / per_second as f64))
            });
        Reading {
            memory_bytes: memory,
            cpu,
        }
    }

    /// `statm`'s second field: the resident pages.
    pub(super) fn resident_pages(statm: &str) -> Option<u64> {
        statm.split_whitespace().nth(1)?.parse().ok()
    }

    /// `stat`'s user and system ticks (fields 14 and 15), read after the program's name, which
    /// is in parentheses and may hold spaces.
    pub(super) fn cpu_ticks(stat: &str) -> Option<u64> {
        let after_name = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = after_name.split_whitespace().collect();
        // After the name, field 3 (the state) is the first: 14 and 15 are at 11 and 12.
        let user: u64 = fields.get(11)?.parse().ok()?;
        let system: u64 = fields.get(12)?.parse().ok()?;
        Some(user + system)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod ps {
    use super::Reading;

    pub(super) fn read(pid: u32) -> Reading {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=,time=", "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let text = output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let mut words = text.split_whitespace();
        let memory = words
            .next()
            .and_then(|kib| kib.parse::<u64>().ok())
            .map(|kib| kib * 1024);
        let cpu = words.next().and_then(super::parse_ps_time);
        Reading {
            memory_bytes: memory,
            cpu,
        }
    }
}

/// `ps`'s processor time: `[[dd-]hh:]mm:ss[.ss]`.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(crate) fn parse_ps_time(text: &str) -> Option<Duration> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, text),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let seconds: f64 = parts.last()?.parse().ok()?;
    let mut total = seconds;
    for (i, part) in parts.iter().rev().skip(1).enumerate() {
        let value: f64 = part.parse().ok()?;
        total += value * if i == 0 { 60.0 } else { 3600.0 };
    }
    total += days as f64 * 86_400.0;
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_times_are_read_in_every_form() {
        assert_eq!(parse_ps_time("00:03"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_ps_time("1:02.50"),
            Some(Duration::from_secs_f64(62.5))
        );
        assert_eq!(parse_ps_time("01:00:01"), Some(Duration::from_secs(3601)));
        assert_eq!(
            parse_ps_time("2-00:00:00"),
            Some(Duration::from_secs(172_800))
        );
        assert_eq!(parse_ps_time("x"), None);
        assert_eq!(parse_ps_time("1:2:3:4"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_files_are_read_even_with_spaces_in_the_name() {
        assert_eq!(linux::resident_pages("12345 678 90 1 0 2 0\n"), Some(678));
        let stat =
            "4242 (spelaion plugin (x)) S 1 4242 4242 0 -1 4194304 100 0 0 0 7 3 0 0 20 0 9 0";
        assert_eq!(linux::cpu_ticks(stat), Some(10));
    }
}
