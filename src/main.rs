mod app;
mod cookies;
mod downloads;
mod events;
mod library;
mod settings;
mod storage;
mod tasks;
mod tools;
mod web;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::run().await
}
