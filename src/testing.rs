//! Test isolation.
//!
//! Everything in this crate resolves its state directory and config from the
//! environment, which is process-global while tests are thread-parallel. A
//! sandbox holds a lock for its lifetime, so tests that touch the environment
//! take turns and never see each other's cords.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const OWNED_VARS: &[&str] = &[
    "ANDON_STATE_DIR",
    "ANDON_CONFIG",
    "ANDON_SCOPE",
    "ANDON_GUARD",
    "ANDON_ELICIT",
    "ANDON_WEBHOOK_URL",
    "ANDON_SESSION_ID",
];

pub struct Sandbox {
    pub dir: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Sandbox {
    pub fn new(name: &str) -> Sandbox {
        // A panicking test leaves the lock poisoned; the next test still wants
        // to run, and it gets a fresh directory anyway.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("andon-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox dir");
        let sandbox = Sandbox { dir, _lock: lock };
        sandbox.set("ANDON_STATE_DIR", sandbox.dir.join("state"));
        sandbox
    }

    pub fn set(&self, key: &str, value: impl AsRef<Path>) {
        assert!(OWNED_VARS.contains(&key), "sandbox does not own {key}");
        // Safe: the lock makes this the only thread touching the environment.
        unsafe { std::env::set_var(key, value.as_ref()) };
    }

    /// Writes a config file and points `ANDON_CONFIG` at it.
    pub fn config(&self, body: &str) {
        let path = self.dir.join("config.json");
        std::fs::write(&path, body).expect("config");
        self.set("ANDON_CONFIG", &path);
    }

    pub fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).expect("fixture");
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for key in OWNED_VARS {
            unsafe { std::env::remove_var(key) };
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Arbitrary JSON, for the invariant that earns its keep here: whatever an
/// agent sends, the cord opens.
pub fn arb_value() -> impl proptest::strategy::Strategy<Value = serde_json::Value> {
    use proptest::prelude::*;
    use serde_json::Value;

    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        any::<f64>()
            .prop_filter("json has no NaN or infinity", |f| f.is_finite())
            .prop_map(|f| serde_json::json!(f)),
        ".*".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 24, 5, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            proptest::collection::vec((".*", inner), 0..5)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}
