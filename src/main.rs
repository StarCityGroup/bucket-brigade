mod app;
mod aws;
mod mask;
mod models;
mod tui;

use anyhow::Result;

use app::App;
use aws::S3Service;

#[tokio::main]
async fn main() -> Result<()> {
    let mut app = App::new();
    let s3 = S3Service::new().await?;

    // Set the initial region to the user's default AWS region
    if let Some(region) = s3.region() {
        app.set_region(Some(region.to_string()));
    }

    if let Err(err) = tui::run(&mut app, &s3).await {
        eprintln!("Application error: {err:#}");
    }
    Ok(())
}
