#![allow(unsafe_code)]

use core::ffi::{c_char, CStr};
use std::collections::HashSet;

use retrovert_host::loader::{PluginKind, PluginSet};

pub(crate) fn supported_extensions(plugins: &PluginSet) -> HashSet<String> {
    let mut result = HashSet::new();
    for loaded in plugins.of_kind(PluginKind::Playback) {
        let Some(callback) = loaded
            .playback()
            .and_then(|plugin| plugin.supported_extensions)
        else {
            continue;
        };
        // SAFETY: the descriptor stays resident in `PluginSet` for the call.
        let raw = unsafe { callback() };
        if raw.is_null() {
            continue;
        }
        // SAFETY: the playback ABI requires a static NUL-terminated result.
        let extensions = unsafe { CStr::from_ptr(raw.cast::<c_char>()) }.to_string_lossy();
        result.extend(
            extensions
                .split([',', '|'])
                .map(str::trim)
                .filter(|extension| !extension.is_empty())
                .map(str::to_ascii_lowercase),
        );
    }
    result
}
