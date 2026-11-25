//! A natural exit (design §2.3 "Natural exit"; PR10a1-CL-13).

use chaldea_core::payload::ExitReason;
use chaldea_pty_runtime::ChildExit;

use super::Rig;

/// PR10a1-CL-13 `natural_exit`: the leader's own exit is recorded `exit` with its code and
/// signal, after the exit op's close proved the tree dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_exit() {
    let rig = Rig::new().await;
    for (code, signal) in [(Some(3), None), (None, Some(15))] {
        let (t, l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
        child.exit(ChildExit { code, signal });
        rig.until("the exit is recorded", || {
            rig.launch(l).exit_reason.is_some()
        })
        .await;
        let row = rig.launch(l);
        assert_eq!(row.exit_reason, Some(ExitReason::Exit));
        assert_eq!((row.exit_code, row.exit_signal), (code, signal));
        assert!(row.death_confirmed);
        let exits: Vec<_> = rig
            .events(t)
            .into_iter()
            .filter(|(k, _)| k == "launch_exited")
            .collect();
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].1["reason"], "exit");
    }
    rig.srv.stop().await;
}
