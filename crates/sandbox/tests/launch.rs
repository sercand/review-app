//! `LocalHost::with_launch` / `BashSession::start_with`: the child starts from an
//! empty environment plus exactly the `Launch`'s env (here: `HOME` must be unset).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::Duration;

use review_sandbox::{Launch, LocalHost, SandboxOps};

const T: Duration = Duration::from_secs(20);

#[tokio::test]
async fn start_with_uses_exactly_the_given_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var_os("PATH").unwrap());
    env.insert("MARK".into(), "42".into());
    let host = LocalHost::with_launch(
        dir.path(),
        Launch {
            program: "bash".into(),
            args: vec![],
            env,
        },
    )
    .unwrap();
    let out = host
        .exec("echo $MARK; echo ${HOME-unset}", false, T)
        .await
        .unwrap();
    assert_eq!(out.output.trim(), "42\nunset");
}
