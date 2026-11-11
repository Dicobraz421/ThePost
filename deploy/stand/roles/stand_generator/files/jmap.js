// Нагрузка JMAP стенда SMB (APP-D Д.3, Д.4) для k6: push (WebSocket, RFC 8887) подключённых устройств, поиск
// (Email/query по тексту), изменение флагов (Email/set keywords). Параметры — переменные окружения от
// run-scenario.sh: SMB_HOST, DOMAIN, USERS, USER_PREFIX, ACCOUNTS (login:пароль), DURATION (с), PUSH_VUS,
// SEARCH_PER_MIN, FLAGS_PER_MIN, WORDS_FILE. Вход — Basic (пароль приложения учётной записи). Ответы 5xx — счётчик
// http_5xx (критерий ×3: отказы только 4xx).
import http from 'k6/http';
import ws from 'k6/ws';
import encoding from 'k6/encoding';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const E = __ENV;
const BASE = `https://${E.SMB_HOST}`;
const USERS = parseInt(E.USERS, 10);
const DURATION = parseInt(E.DURATION, 10);
const WORDS = open(E.WORDS_FILE).split('\n').filter((w) => w.length > 0);
const USING = ['urn:ietf:params:jmap:core', 'urn:ietf:params:jmap:mail'];
const END = Date.now() + DURATION * 1000;
const PASS = {};
open(E.ACCOUNTS)
  .split('\n')
  .forEach((line) => {
    const i = line.indexOf(':');
    if (i > 0) PASS[line.slice(0, i)] = line.slice(i + 1);
  });
const http5xx = new Counter('http_5xx');

function arrival(perMin, exec) {
  // Дробная частота — число вызовов за час.
  return {
    executor: 'constant-arrival-rate',
    rate: Math.max(1, Math.round(parseFloat(perMin) * 60)),
    timeUnit: '1h',
    duration: `${DURATION}s`,
    preAllocatedVUs: 10,
    maxVUs: 200,
    exec,
  };
}

const scenarios = {
  search: arrival(E.SEARCH_PER_MIN, 'search'),
  flags: arrival(E.FLAGS_PER_MIN, 'flags'),
};
if (parseInt(E.PUSH_VUS, 10) > 0) {
  scenarios.push = { executor: 'constant-vus', vus: parseInt(E.PUSH_VUS, 10), duration: `${DURATION}s`, exec: 'push' };
}

export const options = {
  scenarios,
  summaryTrendStats: ['avg', 'p(50)', 'p(95)', 'p(99)', 'max'],
  // Порогов нет: оценка — criteria.py; запись нужна, чтобы счётчик попал в --summary-export.
  thresholds: { http_5xx: ['count>=0'] },
};

function login(n) {
  return `${E.USER_PREFIX}${n}@${E.DOMAIN}`;
}

function auth(n) {
  return { Authorization: `Basic ${encoding.b64encode(`${login(n)}:${PASS[login(n)]}`)}` };
}

function randomUser() {
  return 1 + Math.floor(Math.random() * USERS);
}

const accounts = {};

function account(n) {
  if (!(n in accounts)) {
    const r = http.get(`${BASE}/jmap/session`, { headers: auth(n), tags: { name: 'session' } });
    if (!check(r, { 'session 200': (x) => x.status === 200 })) {
      return null;
    }
    accounts[n] = r.json('primaryAccounts')['urn:ietf:params:jmap:mail'];
  }
  return accounts[n];
}

function api(n, calls, name) {
  const headers = Object.assign({ 'Content-Type': 'application/json' }, auth(n));
  const r = http.post(`${BASE}/jmap/api`, JSON.stringify({ using: USING, methodCalls: calls }), {
    headers,
    tags: { name },
  });
  check(r, { [`${name} 200`]: (x) => x.status === 200 });
  if (r.status >= 500) http5xx.add(1);
  return r;
}

export function search() {
  const n = randomUser();
  const acc = account(n);
  if (acc === null) return;
  const word = WORDS[Math.floor(Math.random() * WORDS.length)];
  api(n, [['Email/query', { accountId: acc, filter: { text: word }, limit: 20 }, 'q']], 'Email/query text');
}

export function flags() {
  const n = randomUser();
  const acc = account(n);
  if (acc === null) return;
  const q = api(n, [['Email/query', { accountId: acc, limit: 20 }, 'q']], 'Email/query');
  if (q.status !== 200) return;
  const ids = q.json('methodResponses.0.1.ids') || [];
  if (ids.length === 0) return;
  const id = ids[Math.floor(Math.random() * ids.length)];
  const update = { [id]: { 'keywords/$flagged': Math.random() < 0.5 ? true : null } };
  api(n, [['Email/set', { accountId: acc, update }, 's']], 'Email/set keywords');
}

export function push() {
  const n = 1 + ((__VU - 1) % USERS);
  const hold = END - Date.now();
  if (hold <= 0) return;
  const params = { headers: Object.assign({ 'Sec-WebSocket-Protocol': 'jmap' }, auth(n)), tags: { name: 'push' } };
  const r = ws.connect(`wss://${E.SMB_HOST}/jmap/ws`, params, (socket) => {
    socket.on('open', () => socket.send(JSON.stringify({ '@type': 'WebSocketPushEnable', dataTypes: null })));
    socket.setTimeout(() => socket.close(), hold);
  });
  check(r, { 'push 101': (x) => x && x.status === 101 });
}
