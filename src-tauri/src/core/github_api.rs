use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use super::skillssh_api::build_http_client;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSkillEntry {
    /// Display name (inferred from directory name or repo name for root-level skills)
    pub name: String,
    /// Path relative to repo root, e.g. "skills/my-skill". Empty string for root-level skill.
    pub path: String,
    /// Description (currently not fetched to save API calls)
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TreeResponse {
    tree: Vec<TreeItem>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct TreeItem {
    path: String,
    #[serde(rename = "type")]
    item_type: String,
}

#[derive(Debug, Deserialize)]
struct ContentItem {
    name: String,
    path: String,
    #[serde(rename = "type")]
    item_type: String,
    download_url: Option<String>,
}

/// List all skills in a GitHub repo by scanning for SKILL.md files using the Trees API.
/// This performs a single API call with no file downloads.
pub fn list_repo_skills(
    owner: &str,
    repo: &str,
    branch: &str,
    proxy_url: Option<&str>,
) -> Result<Vec<RepoSkillEntry>> {
    let client = build_http_client(proxy_url, 30);

    let url = format!(
        "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
        owner, repo, branch
    );

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .context("Failed to fetch repo tree from GitHub API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("GitHub API returned {}: {}", status, body);
    }

    let tree_resp: TreeResponse = resp.json().context("Failed to parse GitHub tree response")?;

    if tree_resp.truncated {
        log::warn!("GitHub tree response was truncated for {}/{}; some skills may be missing", owner, repo);
    }

    let mut skills = Vec::new();
    for item in &tree_resp.tree {
        if item.item_type == "blob"
            && (item.path.ends_with("/SKILL.md") || item.path == "SKILL.md")
        {
            let skill_dir = if item.path == "SKILL.md" {
                String::new()
            } else {
                item.path.trim_end_matches("/SKILL.md").to_string()
            };

            let name = if skill_dir.is_empty() {
                repo.to_string()
            } else {
                skill_dir
                    .split('/')
                    .last()
                    .unwrap_or(&skill_dir)
                    .to_string()
            };

            // Fetch SKILL.md content via raw URL to extract description
            let description = fetch_skill_description(
                &client, owner, repo, branch, &item.path,
            );

            skills.push(RepoSkillEntry {
                name,
                path: skill_dir,
                description,
            });
        }
    }

    Ok(skills)
}

/// Fetch a SKILL.md file via raw.githubusercontent.com and extract the `description` field
/// from its YAML frontmatter. Returns None on any error or if the field is absent.
fn fetch_skill_description(
    client: &reqwest::blocking::Client,
    owner: &str,
    repo: &str,
    branch: &str,
    skill_md_path: &str,
) -> Option<String> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        owner, repo, branch, skill_md_path
    );
    let text = client.get(&url).send().ok()?.text().ok()?;
    parse_frontmatter_description(&text)
}

/// Parse the `description:` field from a YAML frontmatter block (`---` ... `---`).
fn parse_frontmatter_description(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return None;
    }
    let rest = &trimmed[3..];
    let end = rest.find("---")?;
    let yaml_str = &rest[..end];
    // Simple line-by-line scan to avoid pulling in serde_yaml here
    for line in yaml_str.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            let v = value.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Download a single skill directory from a GitHub repo using the Contents API.
/// Only downloads the files in the specified directory (not the entire repo).
/// Returns the path to a temporary directory containing the skill files.
pub fn download_skill_dir(
    owner: &str,
    repo: &str,
    branch: &str,
    skill_path: &str,
    proxy_url: Option<&str>,
) -> Result<PathBuf> {
    let client = build_http_client(proxy_url, 30);
    let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let temp_path = temp_dir.path().to_path_buf();
    temp_dir.keep();

    download_directory_recursive(&client, owner, repo, branch, skill_path, &temp_path)?;

    Ok(temp_path)
}

fn download_directory_recursive(
    client: &reqwest::blocking::Client,
    owner: &str,
    repo: &str,
    branch: &str,
    dir_path: &str,
    local_dir: &Path,
) -> Result<()> {
    let url = if dir_path.is_empty() {
        format!(
            "https://api.github.com/repos/{}/{}/contents?ref={}",
            owner, repo, branch
        )
    } else {
        format!(
            "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
            owner, repo, dir_path, branch
        )
    };

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .with_context(|| format!("Failed to fetch contents of '{}'", dir_path))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!(
            "GitHub API returned {} for path '{}': {}",
            status,
            dir_path,
            body
        );
    }

    let items: Vec<ContentItem> =
        resp.json().with_context(|| format!("Failed to parse contents response for '{}'", dir_path))?;

    fs::create_dir_all(local_dir)?;

    for item in items {
        match item.item_type.as_str() {
            "file" => {
                if let Some(download_url) = &item.download_url {
                    let file_resp = client
                        .get(download_url)
                        .send()
                        .with_context(|| format!("Failed to download '{}'", item.path))?;

                    if file_resp.status().is_success() {
                        let bytes = file_resp.bytes()?;
                        let file_path = local_dir.join(&item.name);
                        fs::write(&file_path, &bytes)
                            .with_context(|| format!("Failed to write '{}'", file_path.display()))?;
                    }
                }
            }
            "dir" => {
                let sub_dir = local_dir.join(&item.name);
                download_directory_recursive(client, owner, repo, branch, &item.path, &sub_dir)?;
            }
            _ => {}
        }
    }

    Ok(())
}
