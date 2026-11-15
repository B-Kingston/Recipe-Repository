//! The shared mailer's helpers: address checks, masking, and the public
//! base URL.

use crate::mail::{Mail, mask_email, valid_email};

#[test]
fn addresses_are_checked_and_masked() {
    for good in [
        "jane@example.com",
        "jane.doe+recipes@mail.example.co.uk",
        "j@x.io",
        "émile@exemple.fr",
    ] {
        assert!(valid_email(good), "{good}");
    }
    for bad in [
        "",
        "jane",
        "jane@",
        "@example.com",
        "jane@example",
        "jane@@example.com",
        "jane doe@example.com",
        "jane@exa mple.com",
        ".jane@example.com",
        "jane..doe@example.com",
        "jane@-example.com",
        "jane@example..com",
        "jane@example.com\n",
        "jane<script>@example.com",
    ] {
        assert!(!valid_email(bad), "{bad:?}");
    }
    assert!(!valid_email(&format!("{}@example.com", "a".repeat(65))));
    assert_eq!(mask_email("jane.doe@example.com"), "j***@example.com");
    assert_eq!(mask_email("émile@exemple.fr"), "é***@exemple.fr");
    assert_eq!(mask_email("nobody"), "***");
}

#[test]
fn public_base_url_is_trimmed_and_checked() {
    let mail = Mail::new(None, None, Some(" https://reciperepository.app/ ".into()));
    assert_eq!(
        mail.base_url.as_deref(),
        Some("https://reciperepository.app")
    );
    assert!(!mail.mailer.enabled());
    assert_eq!(mail.recovery_base(), None, "recovery needs mail");
    assert!(
        Mail::new(None, None, Some("ftp://x".into()))
            .base_url
            .is_none()
    );
    assert!(Mail::new(None, None, Some("  ".into())).base_url.is_none());
    assert!(!Mail::new(Some("  ".into()), None, None).mailer.enabled());
    // Recovery links never come from the Host header, so mail without a
    // public base URL offers no recovery.
    assert_eq!(
        Mail::new(Some("re_x".into()), None, None).recovery_base(),
        None
    );
    assert_eq!(
        Mail::new(Some("re_x".into()), None, Some("https://r.app".into())).recovery_base(),
        Some("https://r.app")
    );
}
