use crate::cloudflared::CmdResult;
use std::sync::mpsc::{TryRecvError, channel};
use std::sync::{Arc, Mutex};

/// A job to run in the background. `run` is invoked on a worker thread.
pub struct Job {
    pub id: u64,
    pub label: String,
    pub run: Box<dyn FnOnce() -> CmdResult + Send>,
}

/// A finished job's result, ready for the UI to poll.
#[derive(Clone)]
pub struct Done {
    pub id: u64,
    pub label: String,
    pub result: CmdResult,
}

#[derive(Clone)]
pub struct Runner {
    queue: Arc<Mutex<Vec<Job>>>,
    new_work: Arc<Mutex<bool>>,
    inbox: Arc<Mutex<std::sync::mpsc::Receiver<Done>>>,
    next_id: Arc<Mutex<u64>>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    pub fn new() -> Self {
        let queue: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));
        let new_work: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let next_id = Arc::new(Mutex::new(1u64));

        let (tx, rx) = channel::<Done>();
        let inbox = Arc::new(Mutex::new(rx));

        let q = queue.clone();
        let nw = new_work.clone();
        std::thread::spawn(move || {
            loop {
                // Wait for work.
                {
                    let mut flag = nw.lock().unwrap();
                    while !*flag {
                        drop(flag);
                        std::thread::sleep(std::time::Duration::from_millis(40));
                        flag = nw.lock().unwrap();
                    }
                    *flag = false;
                }
                let job = {
                    let mut guard = q.lock().unwrap();
                    if guard.is_empty() {
                        continue;
                    }
                    guard.remove(0)
                };
                let result = (job.run)();
                let _ = tx.send(Done {
                    id: job.id,
                    label: job.label,
                    result,
                });
            }
        });

        Self {
            queue,
            new_work,
            inbox,
            next_id,
        }
    }

    /// Queue a job; returns its id.
    pub fn spawn(&self, label: impl Into<String>, run: Box<dyn FnOnce() -> CmdResult + Send>) -> u64 {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let v = *n;
            *n += 1;
            v
        };
        self.queue
            .lock()
            .unwrap()
            .push(Job {
                id,
                label: label.into(),
                run,
            });
        *self.new_work.lock().unwrap() = true;
        id
    }

    /// Drain completed jobs, FIFO order. Non-blocking.
    pub fn drain(&self) -> Vec<Done> {
        let mut out = Vec::new();
        let rx = self.inbox.lock().unwrap();
        loop {
            match rx.try_recv() {
                Ok(d) => out.push(d),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}