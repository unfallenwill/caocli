//! The race, tested where it is written: it is the one place a turn decides that
//! it was cancelled, and the promise it makes about ordering is not visible from
//! outside it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Ran, race};
use crate::ui::doubles::{CancelNow, NoCancel};

/// The effect is polled first, so a cancel that has already fired does not throw
/// away an answer that is ready in the same moment. This is the whole reason the
/// `select!` is `biased`.
#[tokio::test]
async fn a_ready_effect_wins_over_a_cancel_that_already_fired() {
    let mut cancel = CancelNow;
    let outcome = race(&mut cancel, async { "the answer" }).await;
    assert!(
        matches!(outcome, Ran::Finished("the answer")),
        "an effect that is ready keeps its result"
    );
}

/// Nothing but the cancel is ready: that is what a cancelled wait looks like.
#[tokio::test]
async fn a_wait_with_nothing_to_return_ends_as_cancelled() {
    let mut cancel = CancelNow;
    let outcome = race(&mut cancel, std::future::pending::<()>()).await;
    assert!(matches!(outcome, Ran::Cancelled));
}

/// A wait that finishes with nothing cancelled is the ordinary case.
#[tokio::test]
async fn an_effect_that_finishes_on_its_own_is_kept() {
    let mut cancel = NoCancel;
    let outcome = race(&mut cancel, async { 7 }).await;
    assert!(matches!(outcome, Ran::Finished(7)));
}

/// The cancel drops the effect it interrupted — which is what disconnects the
/// stream and what kills a Bash child process (`kill_on_drop`). A turn that only
/// stopped *waiting* would leave both running.
#[tokio::test]
async fn a_cancel_drops_the_effect_it_interrupted() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let guard = DropFlag(Arc::clone(&dropped));
    let effect = async move {
        let _guard = guard;
        std::future::pending::<()>().await
    };

    let mut cancel = CancelNow;
    let outcome = race(&mut cancel, effect).await;
    assert!(matches!(outcome, Ran::Cancelled));
    assert!(
        dropped.load(Ordering::SeqCst),
        "the interrupted effect must have been dropped, not just abandoned"
    );
}
