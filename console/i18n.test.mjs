// The existing rendered journey covers interaction/persistence. These isolated
// cases cover locale negotiation and catalog faults not expressible by a click.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { acceptLanguages, browserLocales, canonicalLocale, createCatalog, formatMessage, intlLocales, localeCookie, matchLocale, recentLocales } from './i18n-core.mjs';
import { contaminationReason } from '../scripts/locales/audit-contamination.mjs';
const manifest = JSON.parse(await readFile(new URL('./locales/manifest.json', import.meta.url)));
const fixture = { ...manifest, locales: manifest.locales.map(entry => ({ ...entry, status: 'enabled' })) };

test('catalog review catches observed German donor fragments in non-German plural branches', () => {
  for(const locale of ['sr-Cyrl','el','bg','tr','et','fr','nl','ru','hy']) {
    for(const value of ['{count} Änderung angefordert','{count} Änderungen angefordert']) {
      assert.equal(contaminationReason(locale,'evidence','changesRequested',value),'german-donor-template');
    }
    assert.equal(contaminationReason(locale,'evidence','commentAdded','коментар zu {count} опција'),'german-donor-template');
  }
  assert.equal(contaminationReason('uk','common','loading','Loading screenshot'),'phrase');
  assert.equal(contaminationReason('uk','common','detail','Your current task and history'),'english-token-cluster');
});
test('catalog review preserves German, valid loans, technical examples and translated messages', () => {
  for(const [locale,namespace,id,value] of [
    ['de','evidence','changesRequested','{count} Änderungen angefordert'],
    ['de','evidence','commentAdded','Kommentar zu {count} Optionen'],
    ['lb','evidence','commentAdded','Bemierkung zu {count} Optiounen'],
    ['tr','shell','view_plan','Plan'],['et','tests','test_532eaa','Test'],
    ['el','evidence','changesRequested','Ζητήθηκαν {count} αλλαγές'],
    ['sr-Cyrl','evidence','commentAdded','Коментар је додат за {count} опције'],
    ['uk','admin','telegram_acdd1e','Telegram'],['uk','glossary','en_ru_pt_br_3c5dc6','en, ru, pt-BR'],
  ]) assert.equal(contaminationReason(locale,namespace,id,value),null,value);
});

test('locale matching preserves script and follows ordered supported browser preferences', () => {
  for (const [input, output] of [['uk-UA','uk'], ['ru-RU','ru'], ['de-AT','de'], ['zh-TW','zh-Hant'], ['zh-HK','zh-Hant'], ['zh-CN','zh-Hans'], ['zh-SG','zh-Hans'], ['zh','zh-Hans'], ['sr-Latn-RS','sr-Latn'], ['sr-RS','sr-Cyrl'], ['no-NO','nb']]) assert.equal(matchLocale(input, fixture.locales), output, input);
  assert.equal(matchLocale('xx-XX', fixture.locales), null);
  assert.equal(matchLocale('../../uk', fixture.locales), null);
  assert.equal(canonicalLocale('zh-hant-tw'), 'zh-Hant-TW');
  assert.deepEqual(browserLocales(['xx-XX','uk-UA','uk','zh-TW'], fixture.locales), ['uk','zh-Hant']);
  assert.deepEqual(recentLocales(['fr','de','fr','ru','ja','ko'], ['uk-UA','de-DE'], fixture.locales), ['fr','de','ru','ja','ko','uk']);
});
test('enabled catalogs match; cookies and weighted headers are validated', () => {
  assert.equal(matchLocale('uk-UA', manifest.locales), 'uk');
  assert.deepEqual(acceptLanguages('fr;q=0.3,uk-UA,ru;q=0,en;q=broken,de;q=0.8'), ['uk-UA','de','fr']);
  assert.equal(localeCookie('dc2_session=opaque; dc2-locale=zh-Hant'), 'zh-Hant');
  assert.equal(localeCookie('dc2-locale=%E0%A4%A'), null);
  assert.equal(localeCookie('dc2-locale=../../other'), null);
});
test('plural messages follow the selected language without evaluating markup', () => {
  const message = { argument: 'count', forms: { one: '{count} задача', few: '{count} задачі', many: '{count} задач', other: '{count} задачі' } };
  assert.equal(formatMessage(message, { count: 1 }, 'uk'), '1 задача');
  assert.equal(formatMessage(message, { count: 2 }, 'uk'), '2 задачі');
  assert.equal(formatMessage(message, { count: 5 }, 'uk'), '5 задач');
  assert.equal(formatMessage('Name: {name}', { name: '<img onerror=alert(1)>' }), 'Name: <img onerror=alert(1)>');
  assert.throws(() => formatMessage('Name: {name}', {}), /Missing/);
});
test('declared Intl fallbacks format unsupported languages without changing English fallback grammar', async () => {
  const m={sourceLocale:'en',locales:[{tag:'en',files:{shell:['en']}},{tag:'qaa',intlFallbacks:['uk'],files:{shell:['qaa']}}]};
  const source={number:'Value: {value}',missing:{argument:'count',forms:{one:'{count} item',other:'{count} items'}}};
  const translated={number:'Значення: {value}'};
  const catalog=createCatalog(m,async file=>file==='en'?source:translated);
  await catalog.ensure('qaa',['shell']);
  assert.deepEqual(intlLocales('qaa',m),['qaa','uk','en']);
  assert.equal(catalog.message('qaa','shell.number',{value:1.5}),'Значення: 1,5');
  assert.equal(catalog.message('qaa','shell.missing',{count:2}),'2 items');
  assert.deepEqual(intlLocales('rm',manifest),['rm','en']);
  m.locales[1].intlFallbacks=['uk','uk','../de',null];
  assert.deepEqual(intlLocales('qaa',m),['qaa','uk','en']);
});
test('multiple fragments merge, concurrent reads deduplicate, and failures remain retryable', async () => {
  let failure = true; let reads = 0;
  const m = { sourceLocale: 'en', locales: [{ tag:'en',files:{ shell:['en/a','en/b'] } },{ tag:'uk',files:{ shell:['uk/a','uk/b'] } }] };
  const data = {'en/a':{hello:'Hello'},'en/b':{count:'Count {n}'},'uk/a':{hello:'Вітаємо'},'uk/b':{count:'Кількість {n}'}};
  const catalog = createCatalog(m, async name => { reads++; if(failure && name === 'uk/b') throw Error('download');return data[name]; });
  await Promise.all([catalog.ensure('uk',['shell']), catalog.ensure('uk',['shell'])]);
  assert.equal(reads,4);
  assert.equal(catalog.message('uk','shell.hello'),'Hello');
  failure=false;await catalog.ensure('uk',['shell']);
  assert.equal(reads,6);assert.equal(catalog.message('uk','shell.count',{n:7}),'Кількість 7');
  await catalog.ensure('uk',['shell']);assert.equal(reads,6);
});
test('duplicate keys are rejected and invalid translated placeholders fall back to English', async () => {
  const m = { sourceLocale:'en',locales:[{tag:'en',files:{shell:['a','b']}},{tag:'uk',files:{shell:['c']}}]};
  await assert.rejects(createCatalog(m,async()=>({x:'x'})).ensure('en',['shell']),/Duplicate/);
  const data={a:{x:'Hello {name}'},b:{y:'Other'},c:{x:'Bad {missing}'}};
  const catalog=createCatalog(m,async name=>data[name]);await catalog.ensure('uk',['shell']);
  assert.equal(catalog.message('uk','shell.x',{name:'Test'}),'Hello Test');
  assert.equal(catalog.message('uk','shell.y'),'Other');
});

test('source validation catches duplicate members without rejecting repeated words or nested names', async () => {
  const { duplicateKeys } = await import('../scripts/locales/json-members.mjs');
  assert.deepEqual(duplicateKeys('{"name":"One","name":"Two"}'), ['name']);
  assert.deepEqual(duplicateKeys('{"x":{"one":"a","one":"b"}}'), ['one']);
  assert.deepEqual(duplicateKeys('{"x":{"name":"One"},"y":{"name":"One"}}'), []);
  assert.deepEqual(duplicateKeys('{"message":"name name name","escaped":"\\\"name\\\""}'), []);
  assert.deepEqual(duplicateKeys('{"list":[{}, {"x":1}],"empty":{}}'), []);
  assert.throws(() => duplicateKeys('{broken JSON}'));
});
