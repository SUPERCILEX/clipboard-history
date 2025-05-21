#![no_main]

use std::mem;

use clipboard_history_client_sdk::ui_actor::{UiEntry, UiEntryCache, ui_entry_};
use libfuzzer_sys::fuzz_target;

fn fuzz(data: &[u8], require_utf8: bool, debug: bool) {
    if data.len() < 8 + 8 + 1 {
        return;
    }

    let start = usize::from_le_bytes(data[0..8].try_into().unwrap());
    let end = usize::from_le_bytes(data[8..16].try_into().unwrap());
    let use_highlight = data[16] == 1;
    let data = &data[17..];

    if use_highlight && (start > end || end > data.len()) {
        return;
    }
    if require_utf8 && str::from_utf8(data).is_err() {
        return;
    }

    if debug {
        println!(
            "use_highlight={use_highlight}, start={start}, end={end}\ndata={}",
            data.escape_ascii()
        );
    }
    let UiEntry { entry: _, cache } = ui_entry_(
        unsafe { mem::zeroed() },
        data,
        "text",
        use_highlight.then_some((start, end)),
    );
    match cache {
        UiEntryCache::HighlightedText {
            one_liner,
            start,
            end,
        } => {
            std::hint::black_box(&one_liner[start..end]);
        }
        UiEntryCache::Text { one_liner: _ } | UiEntryCache::Binary { mime_type: _ } => {}
        UiEntryCache::Image | UiEntryCache::Error(_) => unreachable!(),
    }
}
fuzz_target!(|d| fuzz(d, false, false));
