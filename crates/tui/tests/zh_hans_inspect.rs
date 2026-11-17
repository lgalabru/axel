//! The token inspector in Simplified Chinese: its own process, so the locale
//! is fixed before any text is shown. Rendered at the three sizes the pane
//! adapts to; Chinese text must show, and nothing may be wider than the terminal.

use axel_tui::inspect::{Inspector, fixtures};
use axel_tui::model::InspectData;
use axel_tui::util::text_width;
use axel_tui::view::inspect_view;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];
const NOW_MS: i64 = 1_800_000_000_000;

/// The screen, a string per row; the cell a wide character covers after its
/// own is skipped, so each row reads as a terminal shows it.
fn rows(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < buf.area.width {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                x += u16::try_from(text_width(symbol).max(1)).expect("a cell is one or two wide");
            }
            row
        })
        .collect()
}

fn draw(ins: &Inspector, width: u16, height: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| inspect_view::render(f, ins, f.area())).unwrap();
    let screen = rows(&term);
    for (y, row) in screen.iter().enumerate() {
        assert!(text_width(row) <= usize::from(width), "at {width}×{height}: row {y} is {} cells: {row:?}", text_width(row));
    }
    screen
}

#[test]
fn the_token_inspector_speaks_simplified_chinese_within_the_width() {
    assert!(axel_i18n::init(axel_i18n::Locale::ZhHans), "the locale is fixed before any text is shown");
    let mut ins = Inspector::new(Some(fixtures::THREAD_WEBHOOKS.into()), fixtures::data(NOW_MS));
    // A finished request: its usage is known, so the breakdown is scaled.
    ins.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    for (width, height) in SIZES {
        let all = draw(&ins, width, height).join("\n");
        assert!(all.contains("token 检查器") && all.contains("话题 webhook retries"), "the toolbar at {width}×{height}:\n{all}");
        assert!(all.contains("第 7 轮"), "turns at {width}×{height}:\n{all}");
        assert!(all.contains("工具结果 · Read") && all.contains('█'), "the breakdown at {width}×{height}:\n{all}");
        assert!(all.contains("最大：Read 的工具结果"), "the largest part at {width}×{height}:\n{all}");
        assert!(all.contains("筛选"), "the key help at {width}×{height}:\n{all}");
        if width >= inspect_view::SIDEBAR_MIN_WIDTH {
            assert!(all.contains("按智能体") && all.contains("缓存已重建"), "the sidebar at {width}×{height}:\n{all}");
        }
    }
    let empty = Inspector::new(None, InspectData::default());
    for (width, height) in SIZES {
        let all = draw(&empty, width, height).concat();
        assert!(all.contains("Token 检查器") && all.contains("axel inspect on"), "the empty state at {width}×{height}:\n{all}");
    }
}
