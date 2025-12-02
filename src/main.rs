mod app;
mod aws;
mod mask;
mod models;
mod policy;
mod tui;

use anyhow::Result;

use app::App;
use aws::S3Service;
use policy::PolicyStore;

#[tokio::main]
async fn main() -> Result<()> {
    let mut policy_store = PolicyStore::load_or_default()?;
    let existing_policies = policy_store.policies.clone();
    let mut app = App::new(existing_policies);
    let s3 = S3Service::new().await?;
    if let Err(err) = tui::run(&mut app, &s3, &mut policy_store).await {
        eprintln!("Application error: {err:#}");
    }
    Ok(())
}
