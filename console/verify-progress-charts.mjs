import path from 'node:path';

export async function verifyProgressCharts({ page, daemon, check, scenario, baseUrl, output, settle }) {
  const failures = [];
  const onError = (error) => failures.push(error.message);
  page.on('pageerror', onError);
  const originalViewport = page.viewportSize();
  daemon.setScenario(scenario);
  await page.goto(`${baseUrl}#/progress/r0123456789abcdef`);
  await page.waitForSelector('.progress-bar-line-chart');
  await page.evaluate(() => document.fonts.ready);
  for (const viewport of [{ width: 1398, height: 888 }, { width: 927, height: 873 }, { width: 390, height: 844 }]) {
    await page.setViewportSize(viewport);
    for (const period of ['hour', 'day', 'week']) {
      const prefix = `Progress ${viewport.width} ${period}`;
      daemon.calls.length = 0;
      const response = page.waitForResponse(response => response.url().endsWith('/api/v2/progress.repository') && response.request().postDataJSON()?.period === period);
      await page.click(`[data-progress-period="${period}"]`);
      await settle(period);
      const data = (await (await response).json()).data;
      check(`${prefix}: period control reads the requested real API shape`, daemon.calls.some((call) =>
        call.operation === 'progress.repository' && call.params.period === period));
      for (const scrollToEnd of [false, true]) {
        await page.locator('.progress-chart-scroll').evaluate((element, atEnd) => {
          element.scrollLeft = atEnd ? element.scrollWidth : 0;
        }, scrollToEnd);
        const geometry = await page.locator('.progress-bar-line-chart').evaluateAll((charts) => charts.map((chart) => {
          const headings = [...chart.querySelectorAll('[data-ui-verify-svg-overlap]')];
          const plotted = [...chart.querySelectorAll('.progress-bar,.progress-bar-value')];
          const intersects = (first, second) => Math.min(first.right, second.right) - Math.max(first.left, second.left) > 1
            && Math.min(first.bottom, second.bottom) - Math.max(first.top, second.top) > 1;
          const completed = [...chart.querySelectorAll('.progress-completed-bar')];
          return {
            headings: headings.length,
            renderedScale: chart.getScreenCTM()?.a,
            collisions: headings.flatMap((heading) => plotted.filter((item) =>
              intersects(heading.getBoundingClientRect(), item.getBoundingClientRect()))).length,
            firstMaximum: completed.length > 0 && Number(completed[0].getAttribute('height'))
              === Math.max(...completed.map((bar) => Number(bar.getAttribute('height')))),
            compactValue: chart.querySelector('.progress-bar-value')?.textContent,
          };
        }));
        check(`${prefix}: protected labels clear maximum bars at ${scrollToEnd ? 'end' : 'start'}`,
          geometry.length === 2 && geometry.every((chart) => chart.headings === 2 && chart.collisions === 0 && chart.firstMaximum),
          JSON.stringify(geometry));
        check(`${prefix}: chart text is not compressed below its declared size`, geometry.every((chart) => chart.renderedScale >= .99));
        check(`${prefix}: first planned-line value uses compact thousands`, /5[.,]5\s?k/i.test(geometry[1]?.compactValue || ''));
        check(`${prefix}: complete legend stays visible while the chart pans`, await page.locator('.progress-pulse').evaluate(card => {
          const bounds = card.getBoundingClientRect();
          return [...card.querySelectorAll('.progress-chart-legend span')].every(item => {
            const rect = item.getBoundingClientRect();
            return rect.left >= bounds.left && rect.right <= bounds.right && item.scrollWidth <= item.clientWidth + 1;
          });
        }));
      }
      await page.locator('.progress-chart-scroll').evaluate((element) => { element.scrollLeft = 0; });
      const tooltip = page.locator('#progress-point-tooltip');
      const readValues = () => tooltip.locator('dl > div').evaluateAll(rows => Object.fromEntries(rows.map(row => [row.querySelector('dt').textContent, row.querySelector('dd').textContent])));
      const countBefore = daemon.calls.length;
      for (const [lane, completed, incoming] of [[0, 'tasks_completed', 'tasks_created'], [1, 'planned_lines_completed', 'planned_lines_added']]) {
        const chart = page.locator('.progress-bar-line-chart').nth(lane);
        const first = chart.locator('[data-progress-point="0"]');
        await first.hover(); await tooltip.waitFor({ state: 'visible' });
        const values = Object.values(await readValues());
        check(`${prefix}: lane ${lane + 1} hover shows exact completed and added values`, values[0] === data.series[0][completed].toLocaleString('en-US') && values[1] === data.series[0][incoming].toLocaleString('en-US'));
        const tooltipBounds = await tooltip.boundingBox();
        check(`${prefix}: lane ${lane + 1} tooltip stays inside the viewport`, tooltipBounds.x >= 0 && tooltipBounds.y >= 0 && tooltipBounds.x + tooltipBounds.width <= viewport.width && tooltipBounds.y + tooltipBounds.height <= viewport.height);
        await page.setViewportSize({ width: viewport.width, height: viewport.height + 100 });
        check(`${prefix}: lane ${lane + 1} visible point values survive viewport height changes`, await tooltip.isVisible());
        await page.setViewportSize(viewport);
        await page.locator('.progress-chart-legend').hover();
        check(`${prefix}: lane ${lane + 1} pointer exit dismisses values`, !await tooltip.isVisible());
        await first.hover();
        await first.focus();
        await page.keyboard.press('End');
        await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
        const lastValues = Object.values(await readValues());
        const total = data.series.reduce((sum, point) => sum + point[completed], 0);
        check(`${prefix}: lane ${lane + 1} keyboard reaches exact final bucket and running total`, lastValues[0] === data.series.at(-1)[completed].toLocaleString('en-US') && lastValues[2] === total.toLocaleString('en-US') && await chart.locator('[data-progress-point]:focus').getAttribute('data-progress-point') === String(data.series.length - 1));
        await page.keyboard.press('Home');
        await page.keyboard.press('ArrowRight');
        check(`${prefix}: lane ${lane + 1} zero is shown as zero`, Object.values(await readValues())[0] === data.series[1][completed].toLocaleString('en-US'));
        await page.keyboard.press('Escape');
        check(`${prefix}: lane ${lane + 1} Escape dismisses values`, !await tooltip.isVisible());
        if (viewport.width === 390) {
          await first.tap();
          check(`${prefix}: lane ${lane + 1} touch reveals values`, await tooltip.isVisible());
          await page.locator('.progress-chart-legend').tap();
          check(`${prefix}: lane ${lane + 1} tapping outside dismisses values`, !await tooltip.isVisible());
        }
      }
      check(`${prefix}: point inspection does not refetch the report`, daemon.calls.length === countBefore);
      await page.evaluate(() => window.scrollTo(0, 0));
      check(`${prefix}: no document overflow`, await page.evaluate(() =>
        document.documentElement.scrollWidth <= document.documentElement.clientWidth));
      await page.screenshot({ path: path.join(output, `progress-${viewport.width}-${period}-initial.png`) });
      await page.screenshot({ path: path.join(output, `progress-${viewport.width}-${period}-full.png`), fullPage: true });
    }
  }
  daemon.setScenario({ ...scenario, progressReference: false, progressTokenPartial: true });
  await page.reload();
  await page.locator('[data-progress-evidence="tokens"] .progress-point-target').first().waitFor();
  const tokenChart = page.locator('[data-progress-evidence="tokens"]');
  check('Progress: missing token buckets have no invented point', await tokenChart.locator('[data-progress-point="0"]').count() === 0);
  await tokenChart.locator('[data-progress-point="1"]').hover();
  check('Progress: measured token values remain exact', await page.locator('#progress-point-tooltip dd').first().innerText() === '120,000');
  await tokenChart.locator('[data-progress-point="3"]').focus();
  check('Progress: measured zero tokens remain distinct from missing buckets', await page.locator('#progress-point-tooltip dd').first().innerText() === '0');
  await page.locator('[data-progress-evidence="tests"] [data-progress-point="2"]').focus();
  check('Progress: test points reveal pass rate and measured run counts', await page.locator('#progress-point-tooltip dd').allTextContents().then(values => values.join() === '75%,3,4'));
  await page.keyboard.press('Escape');
  check('Progress: no browser runtime errors', failures.length === 0, JSON.stringify(failures));
  page.off('pageerror', onError);
  await page.setViewportSize(originalViewport);
  daemon.setScenario(scenario);
  daemon.calls.length = 0;
  await page.click('[data-progress-period="day"]');
  await settle('day');
}
