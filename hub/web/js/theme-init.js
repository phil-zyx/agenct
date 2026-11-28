(function () {
  try {
    var saved = localStorage.getItem('agenct:theme');
    if (saved === 'light' || saved === 'dark') {
      document.documentElement.dataset.theme = saved;
    }
  } catch (_) {
    // ignore storage errors
  }
})();
