use crate::env_bool;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn env_bool_accepts_true_and_false_and_uses_default_for_invalid_values() {
    let _lock = ENV_LOCK.lock().unwrap();
    unsafe { std::env::set_var("KINDLE_RECIPES_TEST_BOOL", "false") };
    assert!(!env_bool("KINDLE_RECIPES_TEST_BOOL", true));
    unsafe { std::env::set_var("KINDLE_RECIPES_TEST_BOOL", "true") };
    assert!(env_bool("KINDLE_RECIPES_TEST_BOOL", false));
    unsafe { std::env::set_var("KINDLE_RECIPES_TEST_BOOL", "yes") };
    assert!(env_bool("KINDLE_RECIPES_TEST_BOOL", true));
    unsafe { std::env::remove_var("KINDLE_RECIPES_TEST_BOOL") };
}
