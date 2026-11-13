// Нагрузка календарей и контактов стенда SMB (APP-D Д.3, Д.4; DEP-21, CMP-10) для k6. Календарные операции (CalDAV):
// опрос коллекции PROPFIND (getctag, sync-token), синхронизация sync-collection, вид недели calendar-query, чтение
// события GET, новое событие PUT. Приглашения — событие с участником (планирование iTIP на узле). Контакты (CardDAV):
// PROPFIND, sync-collection, поиск addressbook-query, GET и новая карточка PUT. Для GET берётся ресурс, который уже
// встречался этой VU (PUT или href из ответа 207); пока таких нет, выполняется синхронизация.
// Параметры — переменные окружения от run-scenario.sh: SMB_HOST, DOMAIN, USERS, USER_PREFIX, ACCOUNTS (login:пароль),
// DURATION (с), CALENDAR_PER_MIN, INVITES_PER_MIN, CONTACTS_PER_MIN, MAX_VUS (по умолчанию 100 на сценарий).
// Ступени стресса задаются множителем интенсивности: каждая ступень — отдельный запуск со своим --summary-export.
// Вход — Basic (пароль приложения). Ответы 5xx — счётчик http_5xx.
import http from 'k6/http';
import encoding from 'k6/encoding';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const E = __ENV;
const BASE = `https://${E.SMB_HOST}`;
const USERS = parseInt(E.USERS, 10);
const DURATION = parseInt(E.DURATION, 10);
const MAX_VUS = parseInt(E.MAX_VUS || '100', 10);
const PASS = {};
open(E.ACCOUNTS)
  .split('\n')
  .forEach((line) => {
    const i = line.indexOf(':');
    if (i > 0) PASS[line.slice(0, i)] = line.slice(i + 1);
  });
const http5xx = new Counter('http_5xx');
// Отказ запроса (код не из ожидаемых) по имени; имена — в отчёт criteria.py (dav-summary.json).
const davFail = new Counter('dav_fail');
const NAMES = ['CalDAV PROPFIND', 'CalDAV sync-collection', 'CalDAV calendar-query', 'CalDAV GET', 'CalDAV PUT',
  'CalDAV PUT invite', 'CardDAV PROPFIND', 'CardDAV sync-collection', 'CardDAV addressbook-query', 'CardDAV GET',
  'CardDAV PUT'];
// Подметрики попадают в --summary-export только с порогом: пороги-заглушки, оценка — criteria.py.
const thresholds = { http_5xx: ['count>=0'] };
NAMES.forEach((n) => {
  thresholds[`http_req_duration{name:${n}}`] = ['max>=0'];
  thresholds[`dav_fail{name:${n}}`] = ['count>=0'];
});

function arrival(perMin, exec) {
  return {
    executor: 'constant-arrival-rate',
    rate: Math.max(1, Math.round(parseFloat(perMin) * 60)),
    timeUnit: '1h',
    duration: `${DURATION}s`,
    preAllocatedVUs: Math.min(5, MAX_VUS),
    maxVUs: MAX_VUS,
    exec,
  };
}

export const options = {
  scenarios: {
    calendar: arrival(E.CALENDAR_PER_MIN, 'calendar'),
    invite: arrival(E.INVITES_PER_MIN, 'invite'),
    contacts: arrival(E.CONTACTS_PER_MIN, 'contacts'),
  },
  summaryTrendStats: ['count', 'avg', 'p(50)', 'p(95)', 'p(99)', 'max'],
  thresholds,
};

function login(n) {
  return `${E.USER_PREFIX}${n}@${E.DOMAIN}`;
}

function randomUser() {
  return 1 + Math.floor(Math.random() * USERS);
}

function collectionPath(kind, n) {
  const name = kind === 'calendars' ? 'calendar' : 'contacts';
  return `/dav/${kind}/${encodeURIComponent(login(n)).replace('%40', '@')}/${name}/`;
}

function collection(kind, n) {
  return `${BASE}${collectionPath(kind, n)}`;
}

function request(n, method, url, body, headers, name, ok) {
  const h = Object.assign({ Authorization: `Basic ${encoding.b64encode(`${login(n)}:${PASS[login(n)]}`)}` }, headers);
  const r = http.request(method, url, body, { headers: h, tags: { name } });
  if (!check(r, { [`${name} ${ok.join('/')}`]: (x) => ok.includes(x.status) })) davFail.add(1, { name });
  if (r.status >= 500) http5xx.add(1);
  return r;
}

const XML = { 'Content-Type': 'application/xml; charset=utf-8' };
const tokens = {};
// Ресурсы коллекций, известные этой VU: «kind/n» → пути (не больше KNOWN_MAX, новые вытесняют старые).
const known = {};
const KNOWN_MAX = 50;
const HREF = /<(?:\w+:)?href>([^<]+\.(?:ics|vcf))<\/(?:\w+:)?href>/g;

function remember(kind, n, path) {
  const key = `${kind}/${n}`;
  const list = known[key] || (known[key] = []);
  if (list.indexOf(path) >= 0) return;
  list.push(path);
  if (list.length > KNOWN_MAX) list.shift();
}

function rememberHrefs(kind, n, r) {
  if (r.status !== 207 || !r.body) return;
  let m;
  let i = 0;
  HREF.lastIndex = 0;
  while ((m = HREF.exec(r.body)) !== null && i < KNOWN_MAX) {
    remember(kind, n, m[1].replace(/^https?:\/\/[^/]+/, ''));
    i += 1;
  }
}

// Синхронизация клиента (RFC 6578): с последнего токена учётной записи; первый раз — полная.
function sync(kind, n, name) {
  const key = `${kind}/${n}`;
  const token = tokens[key] ? `<d:sync-token>${tokens[key]}</d:sync-token>` : '<d:sync-token/>';
  const body =
    '<?xml version="1.0" encoding="utf-8"?><d:sync-collection xmlns:d="DAV:">' +
    `${token}<d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>`;
  const r = request(n, 'REPORT', collection(kind, n), body, Object.assign({ Depth: '1' }, XML), name, [207, 403]);
  const m = r.status === 207 && r.body ? r.body.match(/<(?:\w+:)?sync-token>([^<]+)</) : null;
  // 403 valid-sync-token — токен очищен (надгробия старше meta.dav.tombstone_ttl): следующая — полная.
  tokens[key] = m ? m[1] : undefined;
  rememberHrefs(kind, n, r);
}

// Опрос клиента: изменилась ли коллекция (getctag, sync-token) — PROPFIND Depth: 0.
function poll(kind, n, name) {
  const body =
    '<?xml version="1.0" encoding="utf-8"?>' +
    '<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><d:resourcetype/>' +
    '<d:displayname/><cs:getctag/><d:sync-token/></d:prop></d:propfind>';
  request(n, 'PROPFIND', collection(kind, n), body, Object.assign({ Depth: '0' }, XML), name, [207]);
}

// Чтение известного ресурса; пока известных нет — синхронизация (она их и находит).
function read(kind, n, name, syncName) {
  const list = known[`${kind}/${n}`];
  if (!list || list.length === 0) {
    sync(kind, n, syncName);
    return;
  }
  request(n, 'GET', `${BASE}${list[Math.floor(Math.random() * list.length)]}`, null, {}, name, [200]);
}

function stamp(d) {
  return d.toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '');
}

function uid() {
  return `load-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

function vevent(id, organizer, attendee) {
  const start = new Date(Date.now() + Math.floor(Math.random() * 14 * 24) * 3600 * 1000);
  start.setUTCMinutes(0, 0, 0);
  const end = new Date(start.getTime() + 3600 * 1000);
  const people = attendee
    ? `ORGANIZER:mailto:${organizer}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:${organizer}\r\n` +
      `ATTENDEE;RSVP=TRUE;PARTSTAT=NEEDS-ACTION:mailto:${attendee}\r\n`
    : '';
  return (
    'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//The Post//stand load//RU\r\nBEGIN:VEVENT\r\n' +
    `UID:${id}\r\nDTSTAMP:${stamp(new Date())}\r\nDTSTART:${stamp(start)}\r\nDTEND:${stamp(end)}\r\n` +
    `SUMMARY:Нагрузка стенда\r\n${people}END:VEVENT\r\nEND:VCALENDAR\r\n`
  );
}

function putEvent(n, attendee, name) {
  const id = uid();
  const headers = { 'Content-Type': 'text/calendar; charset=utf-8', 'If-None-Match': '*' };
  const path = `${collectionPath('calendars', n)}${id}.ics`;
  const r = request(n, 'PUT', `${BASE}${path}`, vevent(id, login(n), attendee), headers, name, [201, 204]);
  if (r.status === 201 || r.status === 204) remember('calendars', n, path);
}

// Календарная операция (12 из 15 в сутки, Д.3): опрос, синхронизация, вид недели, чтение события или новое событие.
export function calendar() {
  const n = randomUser();
  const x = Math.random();
  if (x < 0.2) {
    poll('calendars', n, 'CalDAV PROPFIND');
  } else if (x < 0.55) {
    sync('calendars', n, 'CalDAV sync-collection');
  } else if (x < 0.7) {
    const from = new Date();
    const to = new Date(from.getTime() + 7 * 24 * 3600 * 1000);
    const body =
      '<?xml version="1.0" encoding="utf-8"?>' +
      '<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">' +
      '<d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR">' +
      `<c:comp-filter name="VEVENT"><c:time-range start="${stamp(from)}" end="${stamp(to)}"/></c:comp-filter>` +
      '</c:comp-filter></c:filter></c:calendar-query>';
    const r = request(n, 'REPORT', collection('calendars', n), body, Object.assign({ Depth: '1' }, XML),
      'CalDAV calendar-query', [207]);
    rememberHrefs('calendars', n, r);
  } else if (x < 0.85) {
    read('calendars', n, 'CalDAV GET', 'CalDAV sync-collection');
  } else {
    putEvent(n, null, 'CalDAV PUT');
  }
}

// Приглашение (3 из 15 в сутки, Д.3): событие с участником — другая учётная запись стенда.
export function invite() {
  const n = randomUser();
  let m = randomUser();
  if (USERS > 1) {
    while (m === n) m = randomUser();
  }
  putEvent(n, login(m), 'CalDAV PUT invite');
}

// Контакты: опрос, синхронизация, поиск по набранным буквам (FN или EMAIL), чтение карточки или новая карточка.
export function contacts() {
  const n = randomUser();
  const x = Math.random();
  if (x < 0.2) {
    poll('addressbooks', n, 'CardDAV PROPFIND');
    return;
  }
  if (x < 0.6) {
    sync('addressbooks', n, 'CardDAV sync-collection');
    return;
  }
  if (x < 0.75) {
    const typed = Math.random().toString(36).slice(2, 4);
    const match = (prop) =>
      `<card:prop-filter name="${prop}"><card:text-match collation="i;unicode-casemap" match-type="contains">` +
      `${typed}</card:text-match></card:prop-filter>`;
    const body =
      '<?xml version="1.0" encoding="utf-8"?>' +
      '<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">' +
      '<d:prop><d:getetag/><card:address-data/></d:prop>' +
      `<card:filter test="anyof">${match('FN')}${match('EMAIL')}</card:filter></card:addressbook-query>`;
    const r = request(n, 'REPORT', collection('addressbooks', n), body, Object.assign({ Depth: '1' }, XML),
      'CardDAV addressbook-query', [207]);
    rememberHrefs('addressbooks', n, r);
    return;
  }
  if (x < 0.85) {
    read('addressbooks', n, 'CardDAV GET', 'CardDAV sync-collection');
    return;
  }
  const id = uid();
  const body =
    'BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//The Post//stand load//RU\r\n' +
    `UID:${id}\r\nFN:Контакт ${id}\r\nN:${id};Контакт;;;\r\nEMAIL;TYPE=WORK:${id}@external.test\r\nEND:VCARD\r\n`;
  const headers = { 'Content-Type': 'text/vcard; charset=utf-8', 'If-None-Match': '*' };
  const path = `${collectionPath('addressbooks', n)}${id}.vcf`;
  const r = request(n, 'PUT', `${BASE}${path}`, body, headers, 'CardDAV PUT', [201, 204]);
  if (r.status === 201 || r.status === 204) remember('addressbooks', n, path);
}
