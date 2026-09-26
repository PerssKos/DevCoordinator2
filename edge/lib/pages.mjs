import { createPageLocalization } from './localization.mjs';

// Self-contained dark-theme HTML pages for the auth/error surfaces.
// No external assets, inline CSS only, and every interpolation is escaped.

const ESCAPE_MAP = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
};

export function escapeHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, (ch) => ESCAPE_MAP[ch]);
}

const CSS = `
:root{color-scheme:dark}
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
html,body{min-height:100%}
body{background:#0b0f15;color:#e7edf5;font:15px/1.6 system-ui,-apple-system,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;display:flex;align-items:center;justify-content:center;padding:24px;background-image:radial-gradient(900px 480px at 50% -8%,rgba(46,90,150,.18),transparent 65%)}
.card{width:100%;max-width:432px;background:#101724;border:1px solid #1f2b3d;border-radius:14px;padding:30px 28px 20px;box-shadow:0 24px 64px rgba(0,0,0,.5)}
.brand{display:flex;align-items:center;gap:10px;margin-bottom:24px}
.brand-name{font-weight:650;font-size:15px;letter-spacing:.2px}
.brand-domain{margin-left:auto;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12px;color:#8fa1b8;border:1px solid #24344b;border-radius:999px;padding:2px 10px;background:#0d1420;white-space:nowrap}
.status-code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:42px;font-weight:700;line-height:1;color:#33445e;letter-spacing:1px;margin-bottom:12px}
h1{font-size:20px;font-weight:650;margin-bottom:8px}
p{color:#a7b4c6;font-size:14px;margin-bottom:14px}
p.small{font-size:12.5px;color:#7787a0;margin-bottom:10px}
a{color:#6ea8ff;text-decoration:none}
a:hover{text-decoration:underline}
a:focus-visible,button:focus-visible,input:focus-visible{outline:2px solid #4c8dff;outline-offset:2px;border-radius:6px}
.note{border-radius:10px;padding:12px 14px;font-size:13.5px;line-height:1.55;margin:0 0 16px}
.note-error{border:1px solid #5a2732;background:#2a141b;color:#ffa3ad}
.note-warn{border:1px solid #57431c;background:#271f0f;color:#e8cf9c}
.note-warn strong{display:block;color:#f5c66d;margin-bottom:6px}
.note-warn ol{margin:8px 0 0 18px}
.note-warn li{margin:6px 0}
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12.5px;background:#0d1420;border:1px solid #24344b;border-radius:6px;padding:1px 6px;color:#c7d4e6;word-break:break-all}
.block{display:block;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:#0d1420;border:1px solid #24344b;border-radius:8px;padding:10px 12px;font-size:12.5px;color:#c7d4e6;word-break:break-all;margin:10px 0;user-select:all}
.meta{display:flex;flex-direction:column;gap:6px;margin:0 0 16px}
.meta-row{display:flex;gap:12px;font-size:13px;align-items:baseline}
.meta-row .k{color:#7787a0;min-width:58px;flex:none}
.meta-row .v{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12.5px;color:#c7d4e6;word-break:break-all}
.btn{display:flex;width:100%;align-items:center;justify-content:center;gap:10px;border-radius:8px;font-weight:600;font-size:14px;padding:10px 16px;cursor:pointer;text-decoration:none;border:1px solid transparent}
.btn:hover{text-decoration:none}
.btn svg{flex:none}
.btn-google{background:#fff;color:#1f1f1f;border-color:#d5d9e0}
.btn-google:hover{background:#f2f4f8}
.btn-google[disabled]{opacity:.4;cursor:not-allowed}
.btn-ghost{background:#152033;color:#dbe6f5;border-color:#2a3b55;margin-top:4px}
.btn-ghost:hover{background:#1a2740}
form{margin:18px 0 8px}
.gap-top{margin-top:14px}
.foot{margin-top:22px;padding-top:14px;border-top:1px solid #1c2837;font-size:11.5px;color:#5d6d85;text-align:center;letter-spacing:.3px}
@media (max-width:480px){body{padding:14px}.card{padding:24px 18px 14px}}
:root{color-scheme:light;--bg:#f6f7f8;--panel:#fff;--surface:#eff2f3;--line:#dce2e4;--fg:#202a30;--muted:#596970;--accent:#087e75}
@media(prefers-color-scheme:dark){:root{color-scheme:dark;--bg:#191d21;--panel:#20262b;--surface:#292f35;--line:#363e45;--fg:#e9eef0;--muted:#a3afb6;--accent:#79d7c6}}
body{background:var(--bg);color:var(--fg);background-image:none}.card{background:var(--panel);border-color:var(--line);box-shadow:0 12px 36px #14252f18}.brand-domain,code,.block,.btn-ghost{background:var(--surface);border-color:var(--line);color:var(--fg)}.btn-ghost:hover{background:var(--panel)}h1{font-size:24px;letter-spacing:-.6px}p,p.small,.meta-row .k,.status-code,.foot{color:var(--muted)}.meta-row .v{color:var(--fg)}.foot{border-color:var(--line)}a{color:var(--accent)}a:focus-visible,button:focus-visible,input:focus-visible{outline-color:var(--accent)}
`.trim();

const MARK_SVG =
  '<svg width="26" height="26" viewBox="0 0 26 26" aria-hidden="true">' +
  '<rect x="1" y="1" width="24" height="24" rx="6.5" fill="#0d1420" stroke="#2c3e57" stroke-width="1.5"/>' +
  '<path d="M7.5 9.5 11 13l-3.5 3.5" stroke="#4c8dff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>' +
  '<path d="M13.5 16.5h5" stroke="#8fa1b8" stroke-width="2" stroke-linecap="round"/>' +
  '</svg>';

const GOOGLE_ICON =
  '<svg width="18" height="18" viewBox="0 0 48 48" aria-hidden="true">' +
  '<path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"/>' +
  '<path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"/>' +
  '<path fill="#FBBC05" d="M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z"/>' +
  '<path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"/>' +
  '</svg>';

export function createPages({ config, localization = createPageLocalization(config.catalogDir), request } = {}) {
  const { locale, direction, t } = localization.forRequest(request);
  const text = (id, params) => escapeHtml(t(id, params));
  const codeText = (id, name, value) => text(id, { [name]: '\uE000' }).replaceAll('\uE000', `<code>${escapeHtml(value)}</code>`);
  const { domain, consoleOrigin } = config;
  function page({ title, body }) {
    return `<!doctype html><html lang="${escapeHtml(locale)}" dir="${direction}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex, nofollow"><title>${escapeHtml(title)} · DevCoordinator2</title><style>${CSS}</style></head><body><main class="card"><div class="brand">${MARK_SVG}<span class="brand-name">DevCoordinator2</span><span class="brand-domain">${escapeHtml(domain)}</span></div>${body}<footer class="foot">DevCoordinator2 — ${escapeHtml(domain)}</footer></main></body></html>`;
  }
  function consoleButton(href = `${consoleOrigin}/`) {
    const safeHref = /^https?:\/\//i.test(String(href)) ? String(href) : `${consoleOrigin}/`;
    return `<a class="btn btn-ghost" href="${escapeHtml(safeHref)}">${text('openConsole')}</a>`;
  }
  function renderLogin({ rt = '', error = '', degraded = false } = {}) {
    const safeRt = typeof rt === 'string' ? rt : '';
    const errors = { 'sign-in is not available right now': 'signInUnavailable', 'sign-in failed; please try again': 'signInFailed' };
    const errorNote = error ? `<div class="note note-error" role="alert">${errors[error] ? text(errors[error]) : escapeHtml(error)}</div>` : '';
    const action = degraded
      ? `<div class="note note-warn"><strong>${text('oauthNotConfigured')}</strong><p>${text('oauthSetup')}</p><ol><li>${text('oauthCreate')}</li><li>${text('oauthRedirect')}<span class="block" dir="ltr">${escapeHtml(consoleOrigin)}/auth/callback</span></li><li>${text('oauthCredentials')}</li></ol></div><button class="btn btn-google" type="button" disabled aria-disabled="true">${GOOGLE_ICON}${text('googleSignIn')}</button>`
      : `<form method="get" action="/auth/start">${safeRt ? '<input type="hidden" name="rt" value="' + escapeHtml(safeRt) + '">' : ''}<button class="btn btn-google" type="submit">${GOOGLE_ICON}${text('googleSignIn')}</button></form>`;
    return { status: error ? 400 : 200, html: page({ title: t('signIn'), body: `<h1>${text('signIn')}</h1><p>${text('signInIntroduction', { domain: '*.' + domain })}</p>${errorNote}${action}<p class="small gap-top">${text('returnAfterSignIn')}</p>` }) };
  }
  function renderDenied({ email = '', resource = '', sessionSet = false, requestToken = '' } = {}) {
    const requestForm = requestToken ? `<form method="post" action="/auth/request-invite"><input type="hidden" name="request_token" value="${escapeHtml(requestToken)}"><button class="btn btn-google" type="submit">${text('requestInvite')}</button></form>` : '';
    return { status: 403, html: page({ title: t('accessDenied'), body: `<div class="status-code">403</div><h1>${text('accessDenied')}</h1><p>${text(resource ? 'accountDenied' : 'accountUninvited')}</p>${email ? '<p><bdi>' + escapeHtml(email) + '</bdi></p>' : ''}${resource ? '<p><bdi>' + escapeHtml(resource) + '</bdi></p>' : ''}<p class="small">${text('askOwner')} ${text(sessionSet ? 'sessionValid' : 'noSession')}</p>${requestForm}<a class="btn btn-ghost" href="${sessionSet ? '/auth/logout' : '/auth/login'}">${text('differentAccount')}</a>` }) };
  }
  function renderInviteResult({ status = 202, duplicate = false, error = '', retryAfter = null } = {}) {
    const title = t(error ? 'requestNotSent' : duplicate ? 'requestPending' : 'inviteRequested');
    return { status, html: page({ title, body: `<div class="status-code">${escapeHtml(error ? status : 202)}</div><h1>${escapeHtml(title)}</h1><p>${error ? escapeHtml(error) : text(duplicate ? 'alreadyPending' : 'ownerCanApprove')}</p>${retryAfter ? '<p class="small">' + text('retrySeconds', { count: retryAfter }) + '</p>' : ''}<a class="btn btn-ghost" href="/">${text('returnResource')}</a>` }) };
  }
  function renderNotFound({ host = '' } = {}) {
    return { status: 404, html: page({ title: t('notFound'), body: `<div class="status-code">404</div><h1>${text('routeNotFound')}</h1><p>${host ? codeText('noRoute', 'host', host) : text('noPage')}</p><p class="small">${text('routeSetup')}</p>${consoleButton()}` }) };
  }
  function renderUpstreamError({ slug = '', kind = '', detail = '', consoleUrl = '' } = {}) {
    const status = kind === 'timeout' ? 504 : 502;
    const message = { connect: 'upstreamConnect', timeout: 'upstreamTimeout', reset: 'upstreamReset', stopped: 'upstreamStopped' }[kind] || 'upstreamUnreachable';
    const body = `<div class="status-code">${status}</div><h1>${text('upstreamUnavailable')}</h1><p>${text(message)}</p><div class="meta"><div class="meta-row"><span class="k">${text('host')}</span><bdi class="v">${escapeHtml(slug ? slug + '.' + domain : domain)}</bdi></div>${kind ? '<div class="meta-row"><span class="k">' + text('cause') + '</span><bdi class="v">' + escapeHtml(kind) + '</bdi></div>' : ''}</div>${detail ? '<span class="block" dir="auto">' + escapeHtml(String(detail)) + '</span>' : ''}<p class="small">${text('restartServer')}</p>${consoleButton(consoleUrl)}`;
    return { status, html: page({ title: t('upstreamUnavailable'), body }) };
  }
  function renderError({ status = 500, title = '', detail = '' } = {}) {
    const safeStatus = Number.isInteger(status) && status >= 400 && status <= 599 ? status : 500;
    const safeTitle = typeof title === 'string' && title ? title : t('somethingWrong');
    return { status: safeStatus, html: page({ title: safeTitle, body: `<div class="status-code">${safeStatus}</div><h1>${escapeHtml(safeTitle)}</h1><p dir="auto">${detail ? escapeHtml(String(detail)) : text('requestFailed')}</p>${consoleButton()}` }) };
  }
  return { renderLogin, renderDenied, renderInviteResult, renderNotFound, renderUpstreamError, renderError, forRequest: request => createPages({ config, localization, request }) };
}
