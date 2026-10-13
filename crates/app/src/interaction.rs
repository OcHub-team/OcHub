//! Navigation policy shared by editors. Views own their data and report only
//! whether it can be left; the shell owns the destination and confirmation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaveState {
    Clean,
    Dirty,
    Busy,
}

impl LeaveState {
    pub(crate) fn new(dirty: bool, busy: bool) -> Self {
        if busy {
            Self::Busy
        } else if dirty {
            Self::Dirty
        } else {
            Self::Clean
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormOwner {
    Settings,
    AppSettings,
    Network,
    Mcp,
    Gateway,
    Themes,
    Tools,
    Remote,
}

/// A user requested cancellation inside a form. Route it through the same
/// guard as sidebar navigation, workspace changes and window close.
pub(crate) struct DiscardForm(pub(crate) FormOwner);
