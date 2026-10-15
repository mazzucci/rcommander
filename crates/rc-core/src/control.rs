//! Remote-control commands: `rcmd ctl …` (or any tool, e.g. a Claude Code mod) sends
//! these to a running instance, which applies them as if the user had done it.
//! Wire format: one JSON object per line, answered by one [`Reply`] line.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Show a directory. For a file, show its directory with the cursor on the file.
    Goto {
        path: String,
        #[serde(default)]
        side: Side,
    },
    /// Open the side-by-side diff of two files.
    Diff { left: String, right: String },
    /// Open a file in the viewer.
    View { path: String },
    /// Select entries matching a wildcard mask such as `*.rs`.
    Select {
        mask: String,
        #[serde(default)]
        side: Side,
    },
    /// Re-read both panels.
    Refresh,
    /// Show a line of text in the message bar.
    Message { text: String },
}

/// Which panel a command applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Active,
    Other,
    Left,
    Right,
}

impl Side {
    /// Panel index (0 = left) given the active panel.
    pub fn index(self, active: usize) -> usize {
        match self {
            Side::Active => active,
            Side::Other => 1 - active,
            Side::Left => 0,
            Side::Right => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok() -> Self {
        Reply { ok: true, error: None }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Reply {
            ok: false,
            error: Some(msg.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let c: Command = serde_json::from_str(r#"{"cmd":"goto","path":"/tmp"}"#).unwrap();
        assert_eq!(
            c,
            Command::Goto {
                path: "/tmp".into(),
                side: Side::Active
            }
        );
        let c: Command = serde_json::from_str(r#"{"cmd":"select","mask":"*.rs","side":"right"}"#).unwrap();
        assert_eq!(
            c,
            Command::Select {
                mask: "*.rs".into(),
                side: Side::Right
            }
        );
        assert_eq!(
            serde_json::to_string(&Command::Refresh).unwrap(),
            r#"{"cmd":"refresh"}"#
        );
        assert_eq!(serde_json::to_string(&Reply::ok()).unwrap(), r#"{"ok":true}"#);
        assert_eq!(Side::Other.index(0), 1);
    }
}
