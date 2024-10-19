
use super::*;

pub struct WeakSender<T> {
    pub(crate) shared: Weak<Shared<T>>,
}

impl<T> WeakSender<T> {
    pub fn upgrade(&self) -> Option<Sender<T>> {
        self.shared
            .upgrade()
            // check that there are still live senders
            .filter(|shared| {
                shared
                    .sender_count
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                        if count == 0 {
                            // all senders are closed already -> don't increase the sender count
                            None
                        } else {
                            // there is still at least one active sender
                            Some(count + 1)
                        }
                    })
                    .is_ok()
            })
            .map(|shared| Sender(shared))
    }
}

impl<T> fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakSender").finish()
    }
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}
