use std::ffi::CStr;
use std::ffi::CString;

// SAFETY: This declares libc's process-global timezone refresh entrypoint with its real C
// signature: `void tzset(void)`. Calls are wrapped in `LibcLocaleBackend::tzset`.
unsafe extern "C" {
    /// Refreshes libc's process-global timezone state from the environment.
    fn tzset();
}

/// Installs a UTF-8 `LC_CTYPE` and environment time zone for this process, or reports why not.
pub(crate) fn initialize_process_locale() -> Result<(), String> {
    initialize_locale(&LibcLocaleBackend)
}

/// Indirection over libc locale calls so the policy is testable without touching the process.
trait LocaleBackend {
    /// Sets `LC_CTYPE` to the named locale, reporting whether libc accepted it.
    fn set_ctype(&self, locale: &str) -> bool;
    /// Sets `LC_CTYPE` from `LC_ALL`, `LC_CTYPE` or `LANG`, reporting whether libc accepted it.
    fn set_ctype_from_environment(&self) -> bool;
    /// Names the character encoding of the active `LC_CTYPE` locale.
    fn codeset(&self) -> Option<String>;
    /// Sets `LC_TIME` from the environment, ignoring failure.
    fn set_time_from_environment(&self);
    /// Rereads the `TZ` environment variable into libc's timezone state.
    fn tzset(&self);
}

/// Prefers a known UTF-8 locale and otherwise requires the environment's locale to be UTF-8.
fn initialize_locale(backend: &impl LocaleBackend) -> Result<(), String> {
    if !backend.set_ctype("en_US.UTF-8") && !backend.set_ctype("C.UTF-8") {
        if !backend.set_ctype_from_environment() {
            return Err("invalid LC_ALL, LC_CTYPE or LANG".to_owned());
        }

        let codeset = backend
            .codeset()
            .unwrap_or_else(|| "unknown".to_owned())
            .to_ascii_lowercase();
        if codeset != "utf-8".to_ascii_lowercase() && codeset != "utf8" {
            return Err(format!(
                "need UTF-8 locale (LC_CTYPE) but have {}",
                backend.codeset().unwrap_or_else(|| "unknown".to_owned())
            ));
        }
    }

    backend.set_time_from_environment();
    backend.tzset();
    Ok(())
}

/// The real backend, calling into libc and mutating this process's global locale.
struct LibcLocaleBackend;

impl LocaleBackend for LibcLocaleBackend {
    /// Sets `LC_CTYPE` through libc `setlocale`.
    fn set_ctype(&self, locale: &str) -> bool {
        setlocale(libc::LC_CTYPE, locale)
    }

    /// Sets `LC_CTYPE` from the environment through libc `setlocale`.
    fn set_ctype_from_environment(&self) -> bool {
        setlocale(libc::LC_CTYPE, "")
    }

    /// Reads the active codeset from `nl_langinfo`, yielding `None` when libc has no answer.
    fn codeset(&self) -> Option<String> {
        // SAFETY: `nl_langinfo(CODESET)` returns either null or a pointer to a
        // process-owned NUL-terminated string for the active locale.
        let codeset = unsafe { libc::nl_langinfo(libc::CODESET) };
        if codeset.is_null() {
            return None;
        }
        Some(
            // SAFETY: The null case is handled above, and libc guarantees a
            // NUL-terminated string for this locale item.
            unsafe { CStr::from_ptr(codeset) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Sets `LC_TIME` from the environment, discarding failure as non-fatal.
    fn set_time_from_environment(&self) {
        let _ = setlocale(libc::LC_TIME, "");
    }

    /// Calls libc `tzset` so later time formatting sees the current `TZ`.
    fn tzset(&self) {
        // SAFETY: `tzset` updates libc process-global timezone state and takes
        // no pointers or Rust-owned resources.
        unsafe { tzset() }
    }
}

/// Calls libc `setlocale`, reporting whether the C library accepted `locale`.
fn setlocale(category: libc::c_int, locale: &str) -> bool {
    let Ok(locale) = CString::new(locale) else {
        return false;
    };
    // SAFETY: `locale` is a live NUL-terminated string for the duration of the
    // call, and `category` is supplied by the libc constants used by callers.
    let result = unsafe { libc::setlocale(category, locale.as_ptr()) };
    !result.is_null()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{LocaleBackend, initialize_locale};
    use std::cell::RefCell;

    #[derive(Default)]
    struct MockLocaleBackend {
        ctype_attempts: RefCell<Vec<String>>,
        ctype_results: RefCell<Vec<bool>>,
        env_result: bool,
        codeset: Option<String>,
        time_calls: RefCell<usize>,
        tzset_calls: RefCell<usize>,
    }

    impl MockLocaleBackend {
        fn with_results(ctype_results: Vec<bool>, env_result: bool, codeset: Option<&str>) -> Self {
            Self {
                ctype_results: RefCell::new(ctype_results),
                env_result,
                codeset: codeset.map(str::to_owned),
                ..Self::default()
            }
        }
    }

    impl LocaleBackend for MockLocaleBackend {
        fn set_ctype(&self, locale: &str) -> bool {
            self.ctype_attempts.borrow_mut().push(locale.to_owned());
            self.ctype_results.borrow_mut().remove(0)
        }

        fn set_ctype_from_environment(&self) -> bool {
            self.ctype_attempts.borrow_mut().push(String::new());
            self.env_result
        }

        fn codeset(&self) -> Option<String> {
            self.codeset.clone()
        }

        fn set_time_from_environment(&self) {
            *self.time_calls.borrow_mut() += 1;
        }

        fn tzset(&self) {
            *self.tzset_calls.borrow_mut() += 1;
        }
    }

    #[test]
    fn builtin_utf8_locale_short_circuits_before_environment_fallback() {
        let backend = MockLocaleBackend::with_results(vec![true], true, Some("UTF-8"));

        assert_eq!(initialize_locale(&backend), Ok(()));
        assert_eq!(backend.ctype_attempts.borrow().as_slice(), ["en_US.UTF-8"]);
        assert_eq!(*backend.time_calls.borrow(), 1);
        assert_eq!(*backend.tzset_calls.borrow(), 1);
    }

    #[test]
    fn c_utf8_fallback_matches_tmux_startup_order() {
        let backend = MockLocaleBackend::with_results(vec![false, true], true, Some("UTF-8"));

        assert_eq!(initialize_locale(&backend), Ok(()));
        assert_eq!(
            backend.ctype_attempts.borrow().as_slice(),
            ["en_US.UTF-8", "C.UTF-8"]
        );
        assert_eq!(*backend.time_calls.borrow(), 1);
        assert_eq!(*backend.tzset_calls.borrow(), 1);
    }

    #[test]
    fn environment_fallback_accepts_utf8_codesets() {
        let backend = MockLocaleBackend::with_results(vec![false, false], true, Some("UTF8"));

        assert_eq!(initialize_locale(&backend), Ok(()));
        assert_eq!(
            backend.ctype_attempts.borrow().as_slice(),
            ["en_US.UTF-8", "C.UTF-8", ""]
        );
    }

    #[test]
    fn environment_fallback_rejects_non_utf8_codesets() {
        let backend = MockLocaleBackend::with_results(vec![false, false], true, Some("ISO-8859-1"));

        assert_eq!(
            initialize_locale(&backend),
            Err("need UTF-8 locale (LC_CTYPE) but have ISO-8859-1".to_owned())
        );
    }

    #[test]
    fn invalid_locale_environment_uses_tmux_error_text() {
        let backend = MockLocaleBackend::with_results(vec![false, false], false, None);

        assert_eq!(
            initialize_locale(&backend),
            Err("invalid LC_ALL, LC_CTYPE or LANG".to_owned())
        );
    }
}
