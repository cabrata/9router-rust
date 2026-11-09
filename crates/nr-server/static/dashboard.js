// Theme toggle, mobile sidebar drawer, and <dialog> modal wiring.
// Port of ThemeToggle.js / themeStore.js (localStorage key "theme":
// light | dark | system) and Sidebar.js's onClose drawer behaviour.
(function () {
  function dark() { return document.documentElement.classList.contains('dark'); }
  function closeNav() {
    var s = document.getElementById('sidebar');
    if (s) { s.classList.remove('open'); }
  }
  document.addEventListener('click', function (e) {
    var t = e.target.closest && e.target.closest('[data-theme-toggle]');
    if (t) {
      var next = dark() ? 'light' : 'dark';
      try { localStorage.setItem('theme', next); } catch (_) {}
      document.documentElement.classList.toggle('dark', next === 'dark');
      return;
    }
    var n = e.target.closest && e.target.closest('[data-nav-toggle]');
    if (n) {
      var side = document.getElementById('sidebar');
      if (side) { side.classList.toggle('open'); }
      return;
    }
    // Sidebar.js closes the drawer when a link is followed.
    if (e.target.closest && e.target.closest('#sidebar a')) { closeNav(); }
    var o = e.target.closest && e.target.closest('[data-modal-open]');
    if (o) { var d = document.getElementById(o.getAttribute('data-modal-open')); if (d && d.showModal) d.showModal(); return; }
    var c = e.target.closest && e.target.closest('[data-modal-close]');
    if (c) { var m = c.closest('dialog'); if (m) m.close(); return; }
    if (e.target.tagName === 'DIALOG' && e.target.classList.contains('modal')) e.target.close(); // backdrop click
  });
})();
