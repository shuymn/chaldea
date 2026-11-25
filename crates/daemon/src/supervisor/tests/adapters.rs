//! `GET /api/adapters` (design §2.8; PR10a1-CL-24).

use chaldea_core::adapter::{CAPABILITIES, HostOs};

use super::{Rig, json};

/// Each host's rows of `CAPABILITIES`, in order, and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn equals_table() {
    for host in [HostOs::Macos, HostOs::Windows] {
        let rig = Rig::with(host, |_| {}).await;
        let r = rig
            .srv
            .send(&rig.srv.req("GET", "/api/adapters").bearer(&rig.owner))
            .await;
        assert_eq!(r.status, 200);
        let v = json(&r);
        assert_eq!(v["host_os"], serde_json::to_value(host).unwrap());
        let want: Vec<serde_json::Value> = CAPABILITIES
            .iter()
            .filter(|c| c.os() == host)
            .map(|c| {
                serde_json::json!({
                    "agent": c.agent(),
                    "pty_encoding": c.pty_encoding(),
                    "first_prompt": c.first_prompt(),
                })
            })
            .collect();
        assert!(!want.is_empty());
        assert_eq!(v["adapters"], serde_json::Value::Array(want), "{host:?}");
        rig.srv.stop().await;
    }
}

/// The guard's rules, as the other read routes answer them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guarded() {
    let rig = Rig::new().await;
    let get = || rig.srv.req("GET", "/api/adapters");
    let cases = [
        (get().bearer(&rig.owner).host(Some("evil.example")), 421),
        (get(), 401),
        (
            get()
                .bearer(&rig.owner)
                .header("Origin", "https://evil.example"),
            403,
        ),
        (
            rig.srv.req("GET", "/api/adapters?x=1").bearer(&rig.owner),
            400,
        ),
    ];
    for (req, status) in cases {
        let r = rig.srv.send(&req).await;
        assert_eq!(r.status, status, "{}", String::from_utf8_lossy(&r.body));
    }
    rig.srv.stop().await;
}
