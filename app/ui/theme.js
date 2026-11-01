// Follows the system's light / dark appearance. The mar-ui tokens are keyed
// on <html data-theme>, so this runs in <head>, before the first paint, and
// again whenever macOS or Windows switches.
(function () {
  const query = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  const apply = () => {
    document.documentElement.dataset.theme = query && query.matches ? "dark" : "light";
  };
  apply();
  if (query && query.addEventListener) query.addEventListener("change", apply);
})();
