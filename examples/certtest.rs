//! Isolates chromiumoxide's certificate-error behaviour from the control loop.
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::security::SetIgnoreCertificateErrorsParams;
use chromiumoxide::Browser;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).expect("url");
    let mode = std::env::args().nth(2).unwrap_or_else(|| "explicit".into());

    // mirror the app: handler must not set the override itself
    let cfg = chromiumoxide::handler::HandlerConfig {
        ignore_https_errors: false,
        ..Default::default()
    };
    let (mut browser, mut handler) =
        Browser::connect_with_config("http://127.0.0.1:9222", cfg).await?;
    tokio::spawn(async move { while let Some(h) = handler.next().await { if h.is_err() { break; } } });

    browser.fetch_targets().await.ok();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // "adopt" mirrors ensure_single_control_page: reuse an existing target rather
    // than creating one.
    let page = if mode.starts_with("adopt") {
        let mut pages = browser.pages().await?;
        println!("  existing pages: {}", pages.len());
        pages.remove(0)
    } else {
        browser.new_page("about:blank").await?
    };
    println!("mode={mode} target={:?}", page.target_id());

    if mode == "explicit" || mode == "adopt-explicit" {
        let r = page.execute(SetIgnoreCertificateErrorsParams::new(true)).await;
        println!("  setIgnoreCertificateErrors -> {:?}", r.map(|_| "ok"));
    }
    // give chromiumoxide's own NetworkManager init chain time to land
    if mode == "wait" {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let r = page.execute(SetIgnoreCertificateErrorsParams::new(true)).await;
        println!("  waited, then set -> {:?}", r.map(|_| "ok"));
    }

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        page.execute(NavigateParams::new(url)),
    ).await;
    match res {
        Ok(Ok(r)) => println!("  navigate -> errorText={:?}", r.result.error_text),
        Ok(Err(e)) => println!("  navigate -> Err({e})"),
        Err(_) => println!("  navigate -> TIMEOUT"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    println!("  url now = {:?}", page.url().await?);
    let title: Option<String> = page.evaluate("document.title").await.ok().and_then(|r| r.into_value().ok());
    println!("  title   = {:?}   <-- 'Privacy error' means the override did NOT apply", title);
    if !mode.starts_with("adopt") { let _ = page.close().await; }
    Ok(())
}
