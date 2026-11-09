// Theme toggle + <dialog> modal wiring. Port of ThemeToggle.js / themeStore.js
// (localStorage key "theme": light | dark | system).
(function () {
  function dark() { return document.documentElement.classList.contains('dark'); }
  document.addEventListener('click', function (e) {
    var t = e.target.closest && e.target.closest('[data-theme-toggle]');
    if (t) {
      var next = dark() ? 'light' : 'dark';
      try { localStorage.setItem('theme', next); } catch (_) {}
      document.documentElement.classList.toggle('dark', next === 'dark');
      return;
    }
    var o = e.target.closest && e.target.closest('[data-modal-open]');
    if (o) { var d = document.getElementById(o.getAttribute('data-modal-open')); if (d && d.showModal) d.showModal(); return; }
    var c = e.target.closest && e.target.closest('[data-modal-close]');
    if (c) { var m = c.closest('dialog'); if (m) m.close(); return; }
    if (e.target.tagName === 'DIALOG' && e.target.classList.contains('modal')) e.target.close(); // backdrop click
  });
})();
