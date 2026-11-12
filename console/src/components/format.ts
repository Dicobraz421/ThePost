// Отображение значений Admin API: моменты времени (UTC на узле — местное время в консоли) и размеры.
import dayjs from "dayjs";

export const when = (v?: string | null) => (v ? dayjs(v).format("DD.MM.YYYY HH:mm") : "—");

const UNITS = ["Б", "КБ", "МБ", "ГБ", "ТБ"];

export function bytes(n?: number | null) {
  if (n === undefined || n === null) return "—";
  let v = n;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${u === 0 ? v : v.toFixed(1)} ${UNITS[u]}`;
}
