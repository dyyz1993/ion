// 会话工作空间 live 页自动验收（Chromium + 真实 LLM）
// 验收点：① 事件 workspace_session_created 触发并到达 ② 卡片渲染 ③ 点击跳转会话
import { createRequire } from 'node:module';
import { execSync } from 'node:child_process';
const require = createRequire('/Users/xuyingzhou/package.json');
const { chromium } = require('playwright');

// 每次验收用全新空仓库：避免历史污染（旧分支/提交会诱导 LLM 过度编排孙会话）
const REPO = execSync(
  `T=$(mktemp -d /tmp/ion-ws-accept-repo.XXXXXX) && cd "$T" && git init -q -b main && ` +
  `git config user.email t@t && git config user.name t && ` +
  `echo "# accept repo" > README.md && git add -A && git commit -qm init && echo "$T"`
).toString().trim();
console.log(`测试仓库: ${REPO}`);

const URL = 'http://127.0.0.1:8789/pages/session-workspace-demo.html?project=' + encodeURIComponent(REPO);
const SHOT = '/tmp/ion_ws_accept';
const results = { consoleErrors: [], steps: [] };
const step = (name, ok, extra = '') => {
  results.steps.push({ name, ok, extra });
  console.log(`${ok ? '✅' : '❌'} ${name}${extra ? ' — ' + extra : ''}`);
};

const browser = await chromium.launch({
  executablePath: '/Applications/Chromium.app/Contents/MacOS/Chromium',
  headless: true,
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on('pageerror', (e) => results.consoleErrors.push('pageerror: ' + e.message));
page.on('console', (m) => { if (m.type() === 'error') results.consoleErrors.push('console: ' + m.text()); });

await page.goto(URL, { waitUntil: 'domcontentloaded' });

// 1) 页面进入 LIVE 模式（create_session + SSE 就绪）
const liveBadge = await page.waitForSelector('text=LIVE · 已连接 Host', { timeout: 20000 }).catch(() => null);
step('页面进入 LIVE 模式', !!liveBadge);
await page.screenshot({ path: `${SHOT}_1_live.png` });

// 2) 发送真实提示词
await page.fill('#composerInput', '帮我写一个贪吃蛇小游戏，这个任务比较独立，放到独立工作空间里做，别影响主分支。');
await page.click('#sendBtn');
step('提示词已发送', true);
await page.screenshot({ path: `${SHOT}_2_sent.png` });

// 3) 监控事件到达（事件流面板）+ 卡片渲染，最长 150s
const t0 = Date.now();
let eventSeen = false, cardSeen = false, cardBranch = '', sawTyping = false;
while (Date.now() - t0 < 150000) {
  if (!sawTyping && await page.$('.typing').catch(() => null)) sawTyping = true;
  const logText = await page.textContent('#logBox').catch(() => '');
  if (!eventSeen && logText.includes('workspace_session_created')) {
    eventSeen = true;
    step('事件 workspace_session_created 到达页面', true, `${((Date.now() - t0) / 1000).toFixed(1)}s`);
  }
  const badge = await page.$('.ws-card .badge.ready, .ws-card .badge.running').catch(() => null);
  const sidebarCount = await page.$$eval('.session-item', els => els.length).catch(() => 0);
  if (!cardSeen && badge && sidebarCount >= 2) {
    cardSeen = true;
    const cardText = await page.textContent('.ws-card');
    cardBranch = (cardText.match(/ion-worker-[0-9a-f]+|feat\/[\w.-]+/) || [''])[0];
    step('真实卡片渲染（ready/running + 侧栏自动新增会话）', true, `分支: ${cardBranch} 侧栏: ${sidebarCount} 条`);
    await page.screenshot({ path: `${SHOT}_3_card.png` });
  }
  if (eventSeen && cardSeen) break;
  await page.waitForTimeout(1500);
}
// 等 creating → ready 稳定
if (cardSeen) {
  await page.waitForSelector('.ws-card .badge.ready', { timeout: 60000 }).catch(() => {});
  await page.screenshot({ path: `${SHOT}_4_ready.png` });
}

// 4) 点击卡片跳转
if (cardSeen) {
  const before = page.url();
  await page.click('text=打开工作树聊天').catch(async () => { await page.click('.ws-card'); });
  await page.waitForTimeout(1500);
  const after = page.url();
  const hashOk = /#\/sessions\/(?!sess_main)/.test(after);
  step('点击卡片跳转到子会话', hashOk, after.split('#')[1] || after);
  await page.screenshot({ path: `${SHOT}_5_jump.png` });
  // 5) 子会话消息区加载
  let chatLoaded = '';
  try {
    await page.waitForFunction(
      () => document.querySelectorAll('.messages-inner .msg').length > 0,
      { timeout: 45000 });
    chatLoaded = await page.textContent('.messages-inner');
  } catch { chatLoaded = await page.textContent('.messages-inner').catch(() => ''); }
  step('子会话聊天页加载（含真实流式输出）', chatLoaded.length > 20, `${chatLoaded.length} 字符`);
  const sidebarAfter = await page.$$eval('.session-item', els => els.length).catch(() => 0);
  step('跳转后侧栏恰好 2 条（主会话+子会话，无串入）', sidebarAfter === 2, `${sidebarAfter} 条`);
}

step('打字指示器出现（LLM 思考期三点动画可见）', sawTyping);

// 汇总
const pass = results.steps.filter(s => s.ok).length, total = results.steps.length;
console.log(`\n═══ 验收: ${pass}/${total} 通过 | JS错误: ${results.consoleErrors.length} ═══`);
if (results.consoleErrors.length) console.log(results.consoleErrors.slice(0, 5).join('\n'));
await browser.close();
process.exit(pass === total && !results.consoleErrors.length ? 0 : 1);
