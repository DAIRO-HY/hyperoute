// Demo script. Shows how the page was delivered by reading the response headers of this script.
(async function () {
  const status = document.getElementById('status');
  try {
    const response = await fetch(document.currentScript ? document.currentScript.src : 'js/app.js', { cache: 'no-store' });
    const encoding = response.headers.get('content-encoding') || 'identity';
    const etag = response.headers.get('etag') || '(none)';
    const cacheControl = response.headers.get('cache-control') || '(none)';
    status.textContent = `JavaScript loaded. content-encoding: ${encoding}, etag: ${etag}, cache-control: ${cacheControl}`;
  } catch (error) {
    status.textContent = `JavaScript loaded, but reading headers failed: ${error}`;
  }
})();

// Some padding so the file is larger than the 1 KB gzip threshold and gets compressed.
// Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.
// Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.
// Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur.
// Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.
// Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.
// Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.
