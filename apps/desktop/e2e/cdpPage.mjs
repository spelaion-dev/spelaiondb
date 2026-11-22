// The app's page over CDP. WebView2 opens its debugging port before it has created its browser
// context and its page, so `connectOverCDP` can answer while `browser.contexts()` is still empty
// (seen as "Cannot read properties of undefined (reading 'context')" in CI): both are waited for
// instead of read at once.

/** The first page of the first browser context, once WebView2 has created them. */
export async function webviewPage(browser, timeout = 30_000) {
  const until = Date.now() + timeout;
  let context = browser.contexts()[0];
  while (!context) {
    if (Date.now() > until) throw new Error(`the app's webview had no browser context ${timeout / 1000} s after its debugging port opened`);
    await new Promise((done) => setTimeout(done, 100));
    context = browser.contexts()[0];
  }
  return context.pages()[0] ?? (await context.waitForEvent("page", { timeout: Math.max(1, until - Date.now()) }));
}
