use crate::{DocType, Session, kernel, kernel_status};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Workspace {
    #[serde(default)]
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) module: String,
    #[serde(default)]
    pub(crate) items: Vec<WorkspaceItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkspaceItem {
    #[serde(default)]
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceLink {
    pub(crate) label: String,
    pub(crate) href: String,
}

pub(crate) async fn workspace_list(
    s: &Session,
) -> std::result::Result<Vec<Workspace>, topcoat::Error> {
    let body = serde_json::json!({
        "limit": 200,
        "order": { "path": "label", "dir": "asc" }
    });
    let (code, out) = kernel::call_async(Some(&s.token), "/read/workspace", &body).await;
    if code != 200 {
        // A failed read is an outage, not "this tenant has no workspaces".
        // Collapsing it to an empty list would render the starter-card fallback
        // as if the workspace directory were genuinely empty, hiding the failure
        // behind a plausible-looking home page. The error must survive the hop.
        return Err(kernel_status(code, &out));
    }
    // A successful *empty* response is the honest "no workspaces yet" — the
    // starter fallback stands in only for that case, never for a failure.
    Ok(serde_json::from_value(out["rows"].clone()).unwrap_or_default())
}

pub(crate) fn workspace_links(
    workspace: &Workspace,
    doctypes: &[DocType],
) -> Vec<WorkspaceLink> {
    workspace
        .items
        .iter()
        .filter_map(|item| {
            let target = doctypes
                .iter()
                .find(|dt| dt.name == item.target && dt.can_read)?;
            let href = match item.kind.as_str() {
                "doctype" if target.issingle => format!("/single/{}", target.name),
                "doctype" => format!("/list/{}", target.name),
                "report"
                    if doctypes
                        .iter()
                        .any(|dt| dt.aggregates.iter().any(|agg| agg.rollup == item.target)) =>
                {
                    format!("/report/{}", item.target)
                }
                _ => return None,
            };
            let label = if item.label.is_empty() {
                target.label_or_name()
            } else {
                item.label.clone()
            };
            Some(WorkspaceLink { label, href })
        })
        .collect()
}

