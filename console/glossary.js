'use strict';

const glossaryState = { scope: null, query: '', language: '', status: '', origin: '', offset: 0, offsets: [], revision: null, highlight: null };
let closeGlossaryDialog = null;

function glossaryRoute() {
  const [route, query = ''] = location.hash.split('?');
  const [, , scope = 'shared', conceptId = ''] = route.split('/');
  const rawRevision = new URLSearchParams(query).get('revision');
  return { scope, conceptId, revision: rawRevision && /^\d+$/.test(rawRevision) ? Number(rawRevision) : null };
}

function glossaryHref(scope = 'shared', conceptId = '', revision = null) {
  return `#/glossary/${encodeURIComponent(scope)}${conceptId ? `/${encodeURIComponent(conceptId)}` : ''}${revision == null ? '' : `?revision=${revision}`}`;
}

function glossaryParams(scope) { return scope === 'shared' ? {} : { repository_id: scope }; }
function glossaryLines(value) { return String(value || '').split('\n').map((line) => line.trim()).filter(Boolean); }
function glossaryOptions(values, selected) { return values.map(([value, label]) => `<option value="${esc(value)}"${value === selected ? ' selected' : ''}>${esc(label)}</option>`).join(''); }
function glossaryLabel(value) { return ({ shared: 'Shared', inherited: 'Inherited', local: 'Project-specific', specialized: 'Project specialization', mandatory: 'Required', default: 'Default', guideline: 'Guidance', draft: 'Draft', approved: 'Approved', deprecated: 'Deprecated' })[value] || value; }
function glossaryLanguage(value) {
  try { return new Intl.DisplayNames([document.documentElement.lang || 'en'], { type: 'language' }).of(value) || value; } catch { return value; }
}

async function viewGlossary() {
  return guard(async () => {
    const route = glossaryRoute();
    if (glossaryState.scope !== route.scope || glossaryState.revision !== route.revision) {
      Object.assign(glossaryState, { scope: route.scope, query: '', language: '', status: '', origin: '', offset: 0, offsets: [], revision: route.revision });
    }
    main.classList.add('glossary-page');
    const params = glossaryParams(route.scope);
    const result = await api(route.conceptId ? 'glossary.get' : 'glossary.list', route.conceptId
        ? { ...params, concept_id: route.conceptId, revision: route.revision }
        : { ...params, revision: route.revision, query: glossaryState.query, language: glossaryState.language || null, status: glossaryState.status || null, origin: glossaryState.origin || null, offset: glossaryState.offset, limit: 25 });
    const profile = result.profile;
    const editable = !!state.who?.administrator && route.revision == null;
    const heading = `<div class="repository-context"><h1>${destinationLink(route.scope === 'shared' ? 'Shared glossary' : 'Glossary', glossaryHref(route.scope))}</h1></div>`;
    const revision = `<span class="muted">Revision ${profile.revision}${route.scope === 'shared' ? '' : ` · Shared revision ${profile.baseline_revision}`}</span>`;
    const historical = route.revision == null ? '' : `<div class="notice">Viewing revision ${profile.revision}. <a href="${glossaryHref(route.scope, route.conceptId)}">Return to the current glossary</a></div>`;
    const tools = `<div class="glossary-tools actions"><button class="btn btn-small" id="glossary-guidance">${editable ? 'Guidance and languages' : 'View guidance'}</button><button class="btn btn-small" id="glossary-history">History</button>${route.scope === 'shared' && state.who?.administrator ? '<button class="btn btn-small" id="glossary-projects">Project adoption</button>' : ''}${profile.adoption_needed && editable ? '<button class="btn btn-small" id="glossary-adopt">Review shared update</button>' : ''}${revision}</div>`;
    if (route.conceptId) {
      main.innerHTML = `${heading}${historical}<a class="glossary-back" href="${glossaryHref(route.scope, '', route.revision)}">← All concepts</a>${glossaryDetail(result.entry, route, editable, profile)}${tools}`;
      $('#glossary-edit')?.addEventListener('click', (event) => glossaryEditor(profile, result.entry, event.currentTarget));
      $('#glossary-inherit')?.addEventListener('click', (event) => glossaryRestore(profile, result.entry, event.currentTarget));
      const related = $('#glossary-related');
      if (related && result.entry.concept.related.length) {
        const details = await Promise.all(result.entry.concept.related.map((identity) => api('glossary.get', { ...params, concept_id: identity, revision: route.revision })));
        related.innerHTML = `<h3>Related concepts</h3><ul>${details.map((detail) => `<li><a href="${glossaryHref(route.scope, detail.entry.concept_id, route.revision)}">${esc(detail.entry.concept.name)}</a></li>`).join('')}</ul>`;
      }
    } else {
      const languages = [...new Set([...profile.languages, ...(profile.available_languages || []), ...result.entries.flatMap((entry) => Object.keys(entry.concept.languages)), ...(glossaryState.language ? [glossaryState.language] : [])])].sort();
      main.innerHTML = `${heading}${historical}<div class="glossary-collection-heading"><h2>Concepts <span class="muted">${result.total}</span></h2>${editable ? '<button class="btn btn-primary" id="glossary-new">Add concept</button>' : ''}</div>
        <form id="glossary-search" class="glossary-filters"><label class="f glossary-query">Find a concept<input name="query" type="search" value="${esc(glossaryState.query)}" placeholder="Term, translation or meaning"></label>
        <label class="f">Language<select name="language">${glossaryOptions([['', 'All languages'], ...languages.map((language) => [language, glossaryLanguage(language)])], glossaryState.language)}</select></label>
        <label class="f">Status<select name="status">${glossaryOptions([['', 'All statuses'], ['approved', 'Approved'], ['draft', 'Draft'], ['deprecated', 'Deprecated']], glossaryState.status)}</select></label>
        <label class="f">Origin<select name="origin">${glossaryOptions([['', 'All origins'], ...(route.scope === 'shared' ? [['shared', 'Shared']] : [['local', 'Project-specific'], ['inherited', 'Inherited'], ['specialized', 'Specialized']])], glossaryState.origin)}</select></label>
        <button class="btn" type="submit">Search</button>${glossaryState.query || glossaryState.language || glossaryState.status || glossaryState.origin ? '<button class="btn" type="button" id="glossary-clear">Clear filters</button>' : ''}</form>
        <div id="glossary-concepts" class="glossary-concepts">${result.entries.length ? result.entries.map((entry) => glossaryCard(entry, route)).join('') : `<div class="notice">${result.total ? 'No concepts on this page.' : glossaryState.query || glossaryState.language || glossaryState.status || glossaryState.origin ? 'No concepts match these filters.' : 'No concepts yet.'}${profile.adoption_needed ? ' A shared glossary revision is available to adopt.' : ''}</div>`}</div>
        <div class="glossary-pagination actions">${glossaryState.offset ? '<button class="btn" id="glossary-previous">Previous concepts</button>' : ''}${result.next_offset != null ? '<button class="btn" id="glossary-next">More concepts</button>' : ''}</div>${tools}`;
      $('#glossary-new')?.addEventListener('click', (event) => glossaryEditor(profile, null, event.currentTarget));
      $('#glossary-search').addEventListener('submit', async (event) => {
        event.preventDefault();
        const data = new FormData(event.currentTarget);
        for (const key of ['query', 'language', 'status', 'origin']) glossaryState[key] = String(data.get(key) || '');
        glossaryState.offset = 0;
        glossaryState.offsets = [];
        await render(); $('#glossary-concepts')?.focus();
      });
      $('#glossary-clear')?.addEventListener('click', () => { Object.assign(glossaryState, { query: '', language: '', status: '', origin: '', offset: 0, offsets: [] }); render(); });
      $('#glossary-next')?.addEventListener('click', () => { glossaryState.offsets.push(glossaryState.offset); glossaryState.offset = result.next_offset; render(); });
      $('#glossary-previous')?.addEventListener('click', () => { glossaryState.offset = glossaryState.offsets.pop() || 0; render(); });
    }
    bindProjectPicker(main);
    $('#glossary-guidance').addEventListener('click', (event) => glossaryGuidance(profile, editable, event.currentTarget));
    $('#glossary-history').addEventListener('click', (event) => glossaryHistory(profile, route.conceptId, event.currentTarget));
    $('#glossary-projects')?.addEventListener('click', (event) => glossaryImpact(event.currentTarget));
    $('#glossary-adopt')?.addEventListener('click', (event) => glossaryAdopt(profile, event.currentTarget));
  })();
}

function glossaryCard(entry, route) {
  const concept = entry.concept;
  const languages = glossaryState.language ? Object.entries(concept.languages).filter(([language]) => language === glossaryState.language) : Object.entries(concept.languages).slice(0, 4);
  return `<article class="glossary-card${glossaryState.highlight === entry.concept_id ? ' glossary-highlight' : ''}"><div class="glossary-card-title"><h3><a href="${glossaryHref(route.scope, entry.concept_id, route.revision)}">${esc(concept.name)}</a></h3><span class="badge">${glossaryLabel(concept.status)}</span></div><p class="glossary-summary">${esc(concept.definition)}</p><div class="glossary-term-preview">${languages.map(([language, term]) => `<span><span class="muted">${esc(language)}</span> <bdi lang="${esc(language)}">${esc(term.preferred)}</bdi>${term.reviewed ? '' : ' <span class="muted">· Needs review</span>'}</span>`).join('')}</div><div class="glossary-card-meta muted">${glossaryLabel(entry.origin)} · ${glossaryLabel(concept.rule)}${Object.keys(concept.languages).length > languages.length ? ` · ${Object.keys(concept.languages).length} languages` : ''}</div></article>`;
}

function glossaryDetail(entry, route, editable, profile) {
  const concept = entry.concept;
  const locked = entry.origin === 'inherited' && concept.rule === 'mandatory';
  return `<section class="glossary-detail"><div class="glossary-collection-heading"><h2>${esc(concept.name)}</h2><div class="actions">${editable && !locked ? `<button class="btn btn-primary" id="glossary-edit">${entry.origin === 'inherited' ? 'Specialize for project' : 'Edit concept'}</button>` : ''}${editable && entry.origin === 'specialized' ? '<button class="btn" id="glossary-inherit">Use shared concept</button>' : ''}</div></div>
    <p class="glossary-definition">${esc(concept.definition)}</p><div class="glossary-card-meta"><span class="badge">${glossaryLabel(concept.status)}</span> ${glossaryLabel(entry.origin)} · ${glossaryLabel(concept.rule)}${entry.inherited_from != null ? ` · <a href="${glossaryHref('shared', entry.concept_id, entry.inherited_from)}">Shared source at revision ${entry.inherited_from}</a>` : ''}</div>
    ${locked ? '<p class="muted">This shared concept is required. Changes belong in its shared source.</p>' : ''}${concept.context ? `<h3>When to use it</h3><p class="glossary-prose">${esc(concept.context)}</p>` : ''}${concept.specialization_reason ? `<h3>Why this project specializes it</h3><p class="glossary-prose">${esc(concept.specialization_reason)}</p>` : ''}
    ${entry.shared_concept ? `<details class="glossary-comparison"><summary>Compare shared meaning</summary><p class="glossary-prose">${esc(entry.shared_concept.definition)}</p></details>` : ''}
    <h3>Languages</h3><div class="glossary-language-grid">${Object.entries(concept.languages).map(([language, term]) => `<section class="glossary-language"><h4>${esc(glossaryLanguage(language))} <span class="muted">${esc(language)}</span></h4><p class="glossary-preferred" lang="${esc(language)}" dir="auto">${esc(term.preferred)}</p><span class="muted">${term.reviewed ? 'Reviewed' : 'Needs language review'}</span>${term.allowed.length ? `<h5>Allowed forms</h5><ul lang="${esc(language)}" dir="auto">${term.allowed.map((value) => `<li>${esc(value)}</li>`).join('')}</ul>` : ''}${term.deprecated.length ? `<h5>Do not use for this meaning</h5><ul lang="${esc(language)}" dir="auto">${term.deprecated.map((value) => `<li>${esc(value)}</li>`).join('')}</ul>` : ''}${term.usage ? `<h5>Usage</h5><p class="glossary-prose" dir="auto">${esc(term.usage)}</p>` : ''}${term.examples.length ? `<h5>Examples</h5><ul lang="${esc(language)}" dir="auto">${term.examples.map((value) => `<li>${esc(value)}</li>`).join('')}</ul>` : ''}</section>`).join('') || '<p class="muted">No language equivalents yet.</p>'}${profile.languages.filter((language) => !concept.languages[language]).map((language) => `<section class="glossary-language"><h4>${esc(glossaryLanguage(language))}</h4><p class="muted">No equivalent yet.</p></section>`).join('')}</div><div id="glossary-related"></div></section>`;
}

function glossaryDialog(title, opener) {
  closeGlossaryDialog?.();
  const dialog = document.createElement('dialog');
  dialog.className = 'glossary-dialog';
  dialog.innerHTML = `<div class="dialog-head"><h2>${esc(title)}</h2><button class="btn btn-small glossary-close" type="button" aria-label="Close ${esc(title)}">×</button></div><div class="glossary-dialog-body"></div>`;
  document.body.appendChild(dialog);
  const previousOverflow = document.documentElement.style.overflow;
  document.documentElement.style.overflow = 'hidden';
  const close = () => { if (!dialog.isConnected) return; dialog.close(); dialog.remove(); document.documentElement.style.overflow = previousOverflow; if (closeGlossaryDialog === close) closeGlossaryDialog = null; if (opener?.isConnected) opener.focus(); };
  closeGlossaryDialog = close;
  $('.glossary-close', dialog).addEventListener('click', close);
  dialog.addEventListener('cancel', (event) => { event.preventDefault(); close(); });
  dialog.showModal();
  return { dialog, body: $('.glossary-dialog-body', dialog), close };
}

function glossaryError(form, error) {
  let output = $('.glossary-form-error', form);
  if (!output) { output = document.createElement('div'); output.className = 'glossary-form-error notice'; output.setAttribute('role', 'alert'); form.prepend(output); }
  output.textContent = error.message;
  output.scrollIntoView({ block: 'nearest' });
  return output;
}

async function glossaryEditor(profile, entry, opener) {
  const scope = profile.repository_id || 'shared';
  const source = entry?.concept || { name: '', definition: '', context: '', status: 'draft', rule: 'default', languages: {}, related: [], specialization_reason: '' };
  const { dialog, body, close } = glossaryDialog(entry ? 'Edit concept' : 'Add concept', opener);
  body.innerHTML = `<form id="glossary-concept-form" class="glossary-edit-form"><div class="glossary-fields"><label class="f">Concept name<input name="name" required maxlength="120" value="${esc(source.name)}"></label><label class="f">Status<select name="status">${glossaryOptions([['draft', 'Draft'], ['approved', 'Approved'], ['deprecated', 'Deprecated']], source.status)}</select></label><label class="f glossary-full">Meaning<textarea name="definition" required minlength="3" maxlength="4000">${esc(source.definition)}</textarea></label><label class="f glossary-full">When to use it<textarea name="context" maxlength="2000">${esc(source.context)}</textarea></label>${scope === 'shared' ? `<label class="f">Project inheritance<select name="rule">${glossaryOptions([['mandatory', 'Required — no project override'], ['default', 'Default — specialization allowed'], ['guideline', 'Guidance']], source.rule)}</select></label>` : ''}${entry?.inherited_from != null ? `<label class="f glossary-full">Why this project needs different terminology<textarea name="specialization_reason" required minlength="3" maxlength="1000">${esc(source.specialization_reason)}</textarea></label>` : ''}</div><p class="muted glossary-review-note" hidden>Changing the meaning requires a new review of unchanged language equivalents after saving.</p><h3>Language equivalents</h3><div id="glossary-language-editors"></div><div class="glossary-add-language"><label class="f">Language tag<input id="glossary-new-language" placeholder="en, ru, pt-BR" maxlength="63"></label><button class="btn" type="button" id="glossary-language-add">Add language</button></div><details class="glossary-related-editor"><summary>Related concepts</summary><div id="glossary-related-selected"></div><label class="f">Find a related concept<input id="glossary-related-query" type="search"></label><button class="btn" type="button" id="glossary-related-search">Find concepts</button><div id="glossary-related-results"></div></details><div class="dialog-actions"><button class="btn glossary-cancel" type="button">Cancel</button><button class="btn btn-primary" type="submit">Save concept</button></div></form>`;
  const form = $('#glossary-concept-form', dialog);
  const languageEditors = $('#glossary-language-editors', dialog);
  const addLanguage = (language, term = {}) => {
    const fieldset = document.createElement('fieldset');
    fieldset.className = 'glossary-language-editor'; fieldset.dataset.language = language;
    fieldset.innerHTML = `<legend>${esc(glossaryLanguage(language))} (${esc(language)})</legend><label class="f">Preferred term<input name="preferred" required maxlength="160" value="${esc(term.preferred || '')}" lang="${esc(language)}" dir="auto"></label><div class="glossary-fields"><label class="f">Allowed forms — one per line<textarea name="allowed" lang="${esc(language)}" dir="auto">${esc((term.allowed || []).join('\n'))}</textarea></label><label class="f">Deprecated forms — one per line<textarea name="deprecated" lang="${esc(language)}" dir="auto">${esc((term.deprecated || []).join('\n'))}</textarea></label></div><label class="f">Language-specific usage<textarea name="usage" maxlength="2000" dir="auto">${esc(term.usage || '')}</textarea></label><label class="f">Examples — one per line<textarea name="examples" lang="${esc(language)}" dir="auto">${esc((term.examples || []).join('\n'))}</textarea></label><div class="glossary-language-actions"><label><input name="reviewed" type="checkbox"${term.reviewed ? ' checked' : ''}> Reviewed for this meaning</label><button class="btn btn-small" type="button" aria-label="Remove ${esc(language)} language">Remove language</button></div>`;
    $('button', fieldset).addEventListener('click', () => { fieldset.remove(); $('#glossary-new-language', dialog).focus(); });
    fieldset.addEventListener('input', (event) => { if (event.target.name !== 'reviewed') $('[name=reviewed]', fieldset).checked = false; });
    languageEditors.appendChild(fieldset);
    return fieldset;
  };
  for (const [language, term] of Object.entries(source.languages)) addLanguage(language, term);
  $('#glossary-language-add', dialog).addEventListener('click', () => {
    const input = $('#glossary-new-language', dialog); const language = input.value.trim().toLowerCase();
    input.setCustomValidity('');
    if (!/^[a-z]{2,8}(?:-[a-z0-9]{1,8})*$/.test(language) || [...languageEditors.children].some((row) => row.dataset.language === language)) {
      input.setCustomValidity('Enter a new language tag, such as en, ru or pt-BR.'); input.reportValidity(); return;
    }
    const row = addLanguage(language); input.value = ''; $('[name=preferred]', row).focus();
  });
  for (const name of ['definition', 'context']) $(`[name=${name}]`, form).addEventListener('input', () => { $('.glossary-review-note', form).hidden = $('[name=definition]', form).value === source.definition && $('[name=context]', form).value === source.context; });
  $('.glossary-cancel', form).addEventListener('click', close);
  const related = new Map(source.related.map((identity) => [identity, null]));
  const renderRelated = () => {
    const selected = $('#glossary-related-selected', form);
    selected.replaceChildren();
    for (const [identity, name] of related) {
      const row = document.createElement('div'); row.className = 'glossary-related-choice';
      const label = document.createElement('span'); label.textContent = name || 'Loading related concept…';
      const button = document.createElement('button'); button.className = 'btn btn-small'; button.type = 'button'; button.textContent = 'Remove'; button.setAttribute('aria-label', `Remove related concept ${name || ''}`);
      button.addEventListener('click', () => { related.delete(identity); renderRelated(); });
      row.append(label, button); selected.append(row);
    }
  };
  renderRelated();
  for (const identity of source.related) {
    api('glossary.get', { ...glossaryParams(scope), concept_id: identity }, false).then((detail) => { if (related.has(identity) && dialog.isConnected) { related.set(identity, detail.entry.concept.name); renderRelated(); } }).catch((error) => { if (dialog.isConnected) glossaryError(form, error); });
  }
  $('#glossary-related-search', form).addEventListener('click', async (event) => {
    const button = event.currentTarget; button.disabled = true;
    try {
      const matches = await api('glossary.list', { ...glossaryParams(scope), query: $('#glossary-related-query', form).value, limit: 10 }, false);
      const results = $('#glossary-related-results', form); results.replaceChildren();
      for (const match of matches.entries.filter((candidate) => candidate.concept_id !== entry?.concept_id && !related.has(candidate.concept_id))) {
        const choice = document.createElement('button'); choice.type = 'button'; choice.className = 'btn'; choice.textContent = `Add ${match.concept.name}`;
        choice.addEventListener('click', () => { related.set(match.concept_id, match.concept.name); renderRelated(); choice.remove(); }); results.appendChild(choice);
      }
      if (!results.children.length) results.textContent = 'No other matching concepts.';
    } catch (error) { glossaryError(form, error); } finally { button.disabled = false; }
  });
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const button = event.submitter; button.disabled = true;
    const data = new FormData(form); const languages = {};
    for (const row of languageEditors.children) languages[row.dataset.language] = { preferred: $('[name=preferred]', row).value, allowed: glossaryLines($('[name=allowed]', row).value), deprecated: glossaryLines($('[name=deprecated]', row).value), usage: $('[name=usage]', row).value, examples: glossaryLines($('[name=examples]', row).value), reviewed: $('[name=reviewed]', row).checked };
    const concept = { name: String(data.get('name')), definition: String(data.get('definition')), context: String(data.get('context')), status: String(data.get('status')), rule: String(data.get('rule') || source.rule), languages, related: [...related.keys()], specialization_reason: String(data.get('specialization_reason') || '') };
    try {
      const saved = await api('glossary.save', { ...glossaryParams(scope), expected_revision: profile.revision, concept_id: entry?.concept_id || null, concept }, false);
      if (!dialog.isConnected) return;
      close();
      glossaryState.highlight = saved.concept_id;
      if (!entry) Object.assign(glossaryState, { query: concept.name, language: '', status: '', origin: '', offset: 0, offsets: [] });
      const href = glossaryHref(scope, entry ? saved.concept_id : '');
      if (location.hash === href) await render(); else location.hash = href;
    } catch (error) {
      const output = glossaryError(form, error);
      if (error.code === 'glossary_conflict' && !$('.glossary-conflict-reload', output)) {
        const reload = document.createElement('button'); reload.type = 'button'; reload.className = 'btn glossary-conflict-reload'; reload.textContent = 'Reload latest and discard this draft';
        reload.addEventListener('click', async () => {
          reload.disabled = true;
          try {
            const latest = await api(entry ? 'glossary.get' : 'glossary.list', { ...glossaryParams(scope), ...(entry ? { concept_id: entry.concept_id } : {}) }, false);
            close(); glossaryEditor(latest.profile, latest.entry || null, opener);
          } catch (failure) { glossaryError(form, failure); reload.disabled = false; }
        }); output.appendChild(reload);
      }
    } finally { button.disabled = false; }
  });
  $('[name=name]', form).focus();
}

function glossaryGuidance(profile, editable, opener) {
  const { dialog, body, close } = glossaryDialog(editable ? 'Guidance and languages' : 'Glossary guidance', opener);
  const inherited = profile.guidelines.filter((item) => item.origin === 'inherited');
  body.innerHTML = `${inherited.length ? `<h3>Inherited guidance</h3>${inherited.map((item) => `<section class="glossary-guideline"><h4>${esc(item.guideline.key)} <span class="badge">${glossaryLabel(item.guideline.rule)}</span></h4><p class="glossary-prose">${esc(item.guideline.text)}</p></section>`).join('')}` : ''}${editable ? `<form id="glossary-settings-form"><label class="f">Glossary languages — comma separated<input name="languages" value="${esc(profile.languages.join(', '))}" placeholder="en, ru, pt-BR"></label><div id="glossary-guideline-editors"></div><button class="btn" id="glossary-guideline-add" type="button">Add guideline</button><div class="dialog-actions"><button class="btn glossary-cancel" type="button">Cancel</button><button class="btn btn-primary" type="submit">Save guidance</button></div></form>` : `<p>Languages: ${profile.languages.map(glossaryLanguage).map(esc).join(', ') || 'Not declared'}</p>${profile.local_guidelines.map((item) => `<section class="glossary-guideline"><h4>${esc(item.key)} <span class="badge">${glossaryLabel(item.rule)}</span></h4><p class="glossary-prose">${esc(item.text)}</p></section>`).join('') || (!inherited.length ? '<p class="muted">No guidance yet.</p>' : '')}`}`;
  if (!editable) return;
  const form = $('#glossary-settings-form', dialog);
  const add = (item = {}) => {
    const row = document.createElement('fieldset'); row.className = 'glossary-guideline-editor';
    row.innerHTML = `<legend>Guideline</legend><label class="f">Name<input name="key" required maxlength="100" value="${esc(item.key || '')}"></label><label class="f">Guidance<textarea name="text" required minlength="3" maxlength="2000">${esc(item.text || '')}</textarea></label><label class="f">Rule<select name="rule">${glossaryOptions([['mandatory', 'Required'], ['default', 'Default'], ['guideline', 'Guidance']], item.rule || 'guideline')}</select></label>${profile.repository_id ? `<label class="f">Reason for specializing a shared guideline<input name="specialization_reason" value="${esc(item.specialization_reason || '')}" maxlength="1000"></label>` : ''}<button class="btn btn-small" type="button">Remove guideline</button>`;
    $('button', row).addEventListener('click', () => { row.remove(); $('#glossary-guideline-add', form).focus(); });
    $('#glossary-guideline-editors', form).appendChild(row); return row;
  };
  profile.local_guidelines.forEach(add);
  $('#glossary-guideline-add', form).addEventListener('click', () => $('[name=key]', add()).focus());
  $('.glossary-cancel', form).addEventListener('click', close);
  form.addEventListener('submit', async (event) => {
    event.preventDefault(); event.submitter.disabled = true;
    const guidelines = [...$('#glossary-guideline-editors', form).children].map((row) => ({ key: $('[name=key]', row).value, text: $('[name=text]', row).value, rule: $('[name=rule]', row).value, specialization_reason: $('[name=specialization_reason]', row)?.value || '' }));
    try {
      await api('glossary.configure', { ...glossaryParams(profile.repository_id || 'shared'), expected_revision: profile.revision, languages: $('[name=languages]', form).value.split(',').map((value) => value.trim()).filter(Boolean), guidelines }, false);
      close(); await render();
    } catch (error) { glossaryError(form, error); } finally { event.submitter.disabled = false; }
  });
  $('[name=languages]', form).focus();
}

async function glossaryHistory(profile, conceptId, opener) {
  const { body, close } = glossaryDialog('Glossary history', opener);
  body.innerHTML = '<p class="muted">Loading revisions…</p>';
  const scope = profile.repository_id || 'shared';
  let before = null;
  const load = async (append = false) => {
    try {
      const history = await api('glossary.history', { ...glossaryParams(scope), concept_id: conceptId || null, before_revision: before, limit: 10 }, false);
      if (!body.isConnected) return;
      if (!append) body.replaceChildren();
      $('.glossary-history-more', body)?.remove();
      for (const revision of history.revisions) {
        const row = document.createElement('article'); row.className = 'glossary-history-row';
        row.innerHTML = `<h3>${esc(revision.summary)}</h3><p class="muted">Revision ${revision.revision} · ${esc(revision.created_at)}</p><a href="${glossaryHref(scope, revision.kind === 'concept' && revision.summary.startsWith('Restored shared') ? '' : revision.concept_id || '', revision.revision)}">View this revision</a>`;
        $('a', row).addEventListener('click', close); body.appendChild(row);
      }
      if (!body.children.length) body.textContent = 'No revisions yet.';
      before = history.next_before_revision;
      if (before != null) { const more = document.createElement('button'); more.className = 'btn glossary-history-more'; more.textContent = 'Earlier revisions'; more.addEventListener('click', () => { more.disabled = true; load(true); }); body.appendChild(more); }
    } catch (error) { glossaryError(body, error); }
  };
  await load();
}

async function glossaryImpact(opener) {
  const { body, close } = glossaryDialog('Project adoption', opener);
  let offset = 0;
  const load = async () => {
    try {
      const result = await api('glossary.impact', { offset, limit: 25 }, false);
      if (!body.isConnected) return;
      $('.glossary-impact-more', body)?.remove();
      if (!offset) body.innerHTML = `<p>Shared revision ${result.shared_revision} · ${result.total} projects</p>`;
      for (const project of result.projects) {
        const row = document.createElement('article'); row.className = 'glossary-history-row';
        row.innerHTML = `<h3><a href="${glossaryHref(project.repository_id)}">${esc(project.display_name)}</a></h3><p>${project.configured ? `Uses shared revision ${project.baseline_revision}${project.baseline_revision === result.shared_revision ? ' · Up to date' : ' · Update available'}` : 'Not configured'}</p>`;
        $('a', row).addEventListener('click', close); body.appendChild(row);
      }
      if (result.next_offset != null) { offset = result.next_offset; const button = document.createElement('button'); button.className = 'btn glossary-impact-more'; button.textContent = 'More projects'; button.addEventListener('click', () => { button.disabled = true; load(); }); body.appendChild(button); }
    } catch (error) { glossaryError(body, error); }
  };
  await load();
}

async function glossaryAdopt(profile, opener) {
  const { body, close } = glossaryDialog('Review shared update', opener);
  body.innerHTML = `<p>This project uses shared revision ${profile.baseline_revision}. Revision ${profile.latest_shared_revision} is available.</p><p><a href="${glossaryHref('shared')}">Inspect the shared glossary</a></p><p>Adoption updates inherited vocabulary, not application messages. Conflicting project specializations must be resolved first.</p><div class="dialog-actions"><button class="btn" id="glossary-adopt-cancel">Cancel</button><button class="btn btn-primary" id="glossary-adopt-save">Adopt revision ${profile.latest_shared_revision}</button></div>`;
  $('a', body).addEventListener('click', close);
  $('#glossary-adopt-cancel', body).addEventListener('click', close);
  $('#glossary-adopt-save', body).addEventListener('click', async (event) => {
    event.currentTarget.disabled = true;
    try {
      await api('glossary.configure', { repository_id: profile.repository_id, expected_revision: profile.revision, baseline_revision: profile.latest_shared_revision, languages: profile.languages, guidelines: profile.local_guidelines }, false);
      close(); await render();
    } catch (error) { glossaryError(body, error); } finally { $('#glossary-adopt-save', body).disabled = false; }
  });
}

async function glossaryRestore(profile, entry, opener) {
  opener.disabled = true;
  try {
    await api('glossary.inherit', { repository_id: profile.repository_id, expected_revision: profile.revision, concept_id: entry.concept_id }, false);
    await render();
  } catch (error) { setBanner(error.message); } finally { opener.disabled = false; }
}
