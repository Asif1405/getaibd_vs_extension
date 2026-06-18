use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdit {
    pub file: String,
    pub diff: Option<String>,
    pub content: Option<String>,
    pub operation: EditOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EditOperation {
    Modify,
    Create,
    Delete,
    Rename { to: String },
}

impl Default for EditOperation {
    fn default() -> Self {
        Self::Modify
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResponse {
    pub plan: String,
    pub edits: Vec<FileEdit>,
    pub verify_commands: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditResult {
    pub file: String,
    pub operation: EditOperation,
    pub success: bool,
    pub error: Option<String>,
    pub original_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResult {
    pub success: bool,
    pub applied: Vec<EditResult>,
    pub failed: Vec<EditResult>,
    pub rollback_available: bool,
}

impl PatchResult {
    pub fn all_succeeded(&self) -> bool {
        self.failed.is_empty()
    }
}
