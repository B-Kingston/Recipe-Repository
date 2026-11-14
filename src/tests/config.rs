use crate::env_bool;
use parking_lot::Mutex;

/// Serialises every test that mutates process environment variables.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn env_bool_reads_the_usual_spellings_and_keeps_the_default_otherwise() {
    let _lock = ENV_LOCK.lock();
    const NAME: &str = "KINDLE_RECIPES_TEST_BOOL";
    for off in ["false", "0", "no", "off", " OFF ", "False"] {
        unsafe { std::env::set_var(NAME, off) };
        assert!(!env_bool(NAME, true), "{off:?}");
    }
    for on in ["true", "1", "yes", "on", " Yes ", "TRUE"] {
        unsafe { std::env::set_var(NAME, on) };
        assert!(env_bool(NAME, false), "{on:?}");
    }
    for unknown in ["", "maybe", "2", "enabled"] {
        unsafe { std::env::set_var(NAME, unknown) };
        assert!(env_bool(NAME, true), "{unknown:?}");
        assert!(!env_bool(NAME, false), "{unknown:?}");
    }
    unsafe { std::env::remove_var(NAME) };
    assert!(env_bool(NAME, true));
    assert!(!env_bool(NAME, false));
}
