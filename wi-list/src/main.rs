use anyhow::Result;
use base64::encode;
use clap::Parser;
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;

/// List work items from Azure DevOps
#[derive(Parser)]
#[command(name = "azure-workitems")]
#[command(about = "List work items from Azure DevOps")]
struct Cli {
    /// Azure DevOps organization
    #[arg(long)]
    organization: String,

    /// Azure DevOps project
    #[arg(long)]
    project: String,

    /// Personal Access Token
    #[arg(long)]
    pat: String,
}

#[derive(Debug, Deserialize)]
struct WorkItemRef {
    id: u64,
    url: String,
}

#[derive(Debug, Deserialize)]
struct WorkItemList {
    workItems: Vec<WorkItemRef>,
}

#[derive(Debug, Deserialize)]
struct WorkItemFields {
    fields: WorkItemData,
}

#[derive(Debug, Deserialize)]
struct WorkItemData {
    #[serde(rename = "System.Id")]
    id: u64,
    #[serde(rename = "System.Title")]
    title: String,
    #[serde(rename = "System.State")]
    state: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let wiql_url = format!(
        "https://dev.azure.com/{}/{}/_apis/wit/wiql?api-version=7.0",
        cli.organization, cli.project
    );

    let wiql_query = serde_json::json!({
        "query": "SELECT [System.Id] FROM WorkItems ORDER BY [System.CreatedDate] DESC"
    });

    let client = Client::builder()
        .user_agent("rust-azure-workitems")
        .build()?;

    let auth_header = format!("Basic {}", encode(format!(":{}", &cli.pat)));

    // Fetch work item IDs
    let resp = client
        .post(&wiql_url)
        .header(AUTHORIZATION, &auth_header)
        .header(CONTENT_TYPE, "application/json")
        .json(&wiql_query)
        .send()
        .await?
        .error_for_status()?
        .json::<WorkItemList>()
        .await?;

    println!("Found {} work items", resp.workItems.len());

    let chunk_size = 200;
    let chunks: Vec<_> = resp.workItems.chunks(chunk_size).collect();

    let pb = ProgressBar::new(resp.workItems.len() as u64);
    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    for chunk in chunks {
        let ids: Vec<String> = chunk.iter().map(|w| w.id.to_string()).collect();
        let batch_url = format!(
            "https://dev.azure.com/{}/{}/_apis/wit/workitems?ids={}&api-version=7.0",
            cli.organization,
            cli.project,
            ids.join(",")
        );

        let batch_resp = client
            .get(&batch_url)
            .header(AUTHORIZATION, &auth_header)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        if let Some(items) = batch_resp.get("value").and_then(|v| v.as_array()) {
            for item in items {
                if let Ok(work_item) = serde_json::from_value::<WorkItemFields>(item.clone()) {
                    println!(
                        "ID: {} | Title: {} | State: {}",
                        work_item.fields.id, work_item.fields.title, work_item.fields.state
                    );
                }
                pb.inc(1);
            }
        }
    }

    pb.finish_with_message("Done fetching work items");

    Ok(())
}

