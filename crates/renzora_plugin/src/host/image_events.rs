//! Bounded OS notifications for compiled plugin images.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use notify_debouncer_full::notify::{self, RecursiveMode, Watcher};

const CAPACITY: usize = 256;

pub(super) struct ImageEvents {
    rx: Mutex<mpsc::Receiver<PathBuf>>,
    overflow: Arc<AtomicBool>,
    _watcher: notify::RecommendedWatcher,
}

impl ImageEvents {
    #[cfg(test)]
    pub(super) fn force_rescan(&self) {
        self.overflow.store(true, Ordering::Release);
    }

    pub(super) fn new(dir: &Path) -> notify::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(CAPACITY);
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = overflow.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                match result {
                    Ok(event) if !event.kind.is_access() => {
                        if event.need_rescan() {
                            callback_overflow.store(true, Ordering::Release);
                        }
                        for path in event.paths {
                            if path.extension().and_then(|ext| ext.to_str())
                                == Some(std::env::consts::DLL_EXTENSION)
                                && tx.try_send(path).is_err()
                            {
                                // Never block the OS callback or silently lose a change.
                                callback_overflow.store(true, Ordering::Release);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => callback_overflow.store(true, Ordering::Release),
                }
            })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(Self {
            rx: Mutex::new(rx),
            overflow,
            _watcher: watcher,
        })
    }

    /// Drains at most one queue's capacity, bounding frame work under churn.
    pub(super) fn drain(&self, mut changed: impl FnMut(PathBuf)) -> bool {
        let Ok(rx) = self.rx.lock() else {
            return true;
        };
        for path in rx.try_iter().take(CAPACITY) {
            changed(path);
        }
        self.overflow.swap(false, Ordering::AcqRel)
    }
}
