use crate::model::{ExtraUsageBudget, ProviderSnapshot, UsageWindow};

use super::{
    CARD_GAP, CARD_HEADER_HEIGHT, CARD_PAD_BOTTOM, CARD_PAD_TOP, EXTRA_USAGE_ROW_HEIGHT,
    HISTORY_ROW_HEIGHT, MESSAGE_ROW_HEIGHT, MODEL_ROW_HEIGHT, WINDOW_ROW_HEIGHT,
};

pub(super) enum CardRow<'a> {
    History,
    Window(&'a UsageWindow),
    Models(&'a [UsageWindow]),
    ExtraUsage(&'a ExtraUsageBudget),
    Message(Option<&'a str>),
}

impl CardRow<'_> {
    fn height(&self) -> i32 {
        match self {
            Self::History => HISTORY_ROW_HEIGHT,
            Self::Window(_) => WINDOW_ROW_HEIGHT,
            Self::Models(_) => MODEL_ROW_HEIGHT,
            Self::ExtraUsage(_) => EXTRA_USAGE_ROW_HEIGHT,
            Self::Message(_) => MESSAGE_ROW_HEIGHT,
        }
    }
}

pub(super) struct PositionedRow<'a> {
    pub top: i32,
    pub content: CardRow<'a>,
}

pub(super) struct CardLayout<'a> {
    pub rows: Vec<PositionedRow<'a>>,
    pub height: i32,
}

impl<'a> CardLayout<'a> {
    pub fn new(snapshot: &'a ProviderSnapshot) -> Self {
        let mut rows = Vec::new();
        if snapshot.from_session_history {
            rows.push(CardRow::History);
        }
        let usage_start = rows.len();
        for window in [snapshot.session.as_ref(), snapshot.weekly.as_ref()]
            .into_iter()
            .flatten()
        {
            rows.push(CardRow::Window(window));
        }
        if !snapshot.model_windows.is_empty() {
            rows.push(CardRow::Models(&snapshot.model_windows));
        }
        if let Some(budget) = snapshot.extra_usage.as_ref() {
            rows.push(CardRow::ExtraUsage(budget));
        }
        if rows.len() == usage_start || snapshot.error.is_some() {
            rows.push(CardRow::Message(snapshot.error.as_deref()));
        }
        let mut top = CARD_PAD_TOP + CARD_HEADER_HEIGHT;
        let rows = rows
            .into_iter()
            .map(|content| {
                let row_top = top;
                top += content.height();
                PositionedRow {
                    top: row_top,
                    content,
                }
            })
            .collect();
        Self {
            rows,
            height: top + CARD_PAD_BOTTOM + CARD_GAP,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    #[test]
    fn card_rows_include_history_usage_and_errors_in_display_order() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Codex);
        snapshot.from_session_history = true;
        snapshot.session = Some(UsageWindow::new("Session", 10.0, None));
        snapshot.weekly = Some(UsageWindow::new("Weekly", 20.0, None));
        snapshot
            .model_windows
            .push(UsageWindow::new("Model", 30.0, None));
        snapshot.extra_usage = Some(ExtraUsageBudget::new(100, Some(200), "USD", 2));
        snapshot.error = Some("Refresh failed".into());
        let layout = CardLayout::new(&snapshot);
        assert!(matches!(layout.rows.as_slice(), [
            PositionedRow { content: CardRow::History, .. },
            PositionedRow { content: CardRow::Window(session), .. },
            PositionedRow { content: CardRow::Window(weekly), .. },
            PositionedRow { content: CardRow::Models(_), .. },
            PositionedRow { content: CardRow::ExtraUsage(_), .. },
            PositionedRow { content: CardRow::Message(Some("Refresh failed")), .. },
        ] if session.label == "Session" && weekly.label == "Weekly"));
        assert_eq!(
            layout.rows.iter().map(|row| row.top).collect::<Vec<_>>(),
            [42, 62, 108, 154, 184, 214]
        );
        assert_eq!(layout.height, 286);
    }

    #[test]
    fn empty_card_keeps_a_message_row_even_with_history() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Codex);
        for history in [false, true] {
            snapshot.from_session_history = history;
            let layout = CardLayout::new(&snapshot);
            assert!(matches!(
                layout.rows.last().unwrap().content,
                CardRow::Message(None)
            ));
        }
    }
}
