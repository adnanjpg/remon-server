//! Probe loader: concurrent reload safety.

use super::TestApp;

/// Regression for the reload race: two unserialized `load_and_spawn`
/// passes could both see a probe as not-running, both spawn, and the
/// second registry insert dropped the first task's JoinHandle detached —
/// a zombie loop firing child processes forever. With the pass mutex the
/// second pass must observe the first one's task and leave it alone.
#[tokio::test]
async fn concurrent_reloads_keep_one_task_per_probe() {
    let app = TestApp::spawn().await;

    let dir = std::env::temp_dir().join(format!("remon-probes-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp probes dir");
    std::fs::write(
        dir.join("racer.yaml"),
        "name: racer\nenabled: true\ninterval: \"1h\"\ncommand: [\"true\"]\n",
    )
    .expect("write manifest");

    let (a, b) = tokio::join!(
        crate::probes::scheduler::load_and_spawn(
            &dir,
            app.state.probe_registry.clone(),
            app.state.db.clone(),
        ),
        crate::probes::scheduler::load_and_spawn(
            &dir,
            app.state.probe_registry.clone(),
            app.state.db.clone(),
        ),
    );
    assert_eq!(a.loaded, vec!["racer".to_string()]);
    assert_eq!(b.loaded, vec!["racer".to_string()]);

    {
        let reg = app.state.probe_registry.read().await;
        assert_eq!(reg.probes.len(), 1);
        let entry = reg.probes.get("racer").expect("registry entry");
        let task = entry.task.as_ref().expect("task handle");
        assert!(!task.is_finished(), "the surviving task must be live");
        task.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
