//! Install / download progress bars (TTY only).

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

const SHOW_INFLIGHT: usize = 3;

pub(crate) struct InstallProgress {
    multi: MultiProgress,
    packages: ProgressBar,
    bytes: ProgressBar,
    inflight: Mutex<Vec<String>>,
}

impl InstallProgress {
    pub(crate) fn maybe(total_packages: u64) -> Option<Self> {
        if total_packages == 0 || !want_progress() {
            return None;
        }

        let multi = MultiProgress::new();
        let packages = multi.add(ProgressBar::new(total_packages));
        packages.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {prefix:.bold.dim} [{bar:28.cyan/blue}] {pos}/{len} {wide_msg}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
        );
        packages.set_prefix("Install");
        packages.enable_steady_tick(Duration::from_millis(80));

        let bytes = multi.add(ProgressBar::new_spinner());
        bytes.set_style(
            ProgressStyle::with_template("{spinner:.green} {bytes}  {bytes_per_sec}  {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bytes.enable_steady_tick(Duration::from_millis(80));
        bytes.set_message("downloaded");

        Some(Self {
            multi,
            packages,
            bytes,
            inflight: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn begin(&self, name: &str) {
        if let Ok(mut g) = self.inflight.lock() {
            g.push(name.to_string());
            self.refresh_msg(&g);
        }
    }

    pub(crate) fn end(&self, name: &str) {
        if let Ok(mut g) = self.inflight.lock() {
            if let Some(i) = g.iter().position(|n| n == name) {
                g.remove(i);
            }
            self.refresh_msg(&g);
        }
        self.packages.inc(1);
    }

    pub(crate) fn add_bytes(&self, n: u64) {
        if n > 0 {
            self.bytes.inc(n);
        }
    }

    pub(crate) fn suspend<R>(&self, f: impl FnOnce() -> R) -> R {
        self.multi.suspend(f)
    }

    pub(crate) fn finish(&self) {
        self.packages.finish_and_clear();
        self.bytes.finish_and_clear();
    }

    fn refresh_msg(&self, names: &[String]) {
        let extra = names.len().saturating_sub(SHOW_INFLIGHT);
        let shown = names
            .iter()
            .take(SHOW_INFLIGHT)
            .cloned()
            .collect::<Vec<_>>()
            .join("  ");
        let msg = if extra > 0 {
            format!("{shown}  +{extra}")
        } else {
            shown
        };
        self.packages.set_message(msg);
    }
}

fn want_progress() -> bool {
    if std::env::var_os("COMPOSER_RS_NO_PROGRESS").is_some() {
        return false;
    }
    match std::env::var("CI") {
        Ok(v) if v != "0" && !v.eq_ignore_ascii_case("false") => return false,
        _ => {}
    }
    std::io::stderr().is_terminal()
}
