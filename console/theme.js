(() => {
  const preference = window.matchMedia('(prefers-color-scheme: dark)');
  let selected;
  try { selected = localStorage.getItem('dc2-theme'); } catch {}
  const apply = (theme) => {
    document.documentElement.dataset.theme = theme;
    const toggle = document.getElementById('theme-toggle');
    if (toggle) {
      if (window.DevCoordinatorI18n?.manifest) {
        window.DevCoordinatorI18n.text(toggle, theme === 'dark' ? 'shell.lightMode' : 'shell.darkMode');
        window.DevCoordinatorI18n.text(toggle, theme === 'dark' ? 'shell.switchLight' : 'shell.switchDark', {}, 'aria-label');
      } else {
        toggle.textContent = theme === 'dark' ? 'Light mode' : 'Dark mode';
        toggle.setAttribute('aria-label', `Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`);
      }
    }
  };
  apply(selected === 'light' || selected === 'dark' ? selected : preference.matches ? 'dark' : 'light');
  document.addEventListener('dc2:localechange', () => apply(document.documentElement.dataset.theme));
  document.addEventListener('DOMContentLoaded', () => {
    apply(document.documentElement.dataset.theme);
    document.getElementById('theme-toggle')?.addEventListener('click', () => {
      selected = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
      try { localStorage.setItem('dc2-theme', selected); } catch {}
      apply(selected);
    });
  });
  preference.addEventListener('change', () => {
    if (selected !== 'light' && selected !== 'dark') apply(preference.matches ? 'dark' : 'light');
  });
})();
