// NOAA's approximate solar equations, evaluated in UTC (no device timezone dependency).
// https://gml.noaa.gov/grad/solcalc/solareqns.PDF
export const LONGYEARBYEN = Object.freeze({
  latitude: 78.2232,
  longitude: 15.6469,
});
const RADIANS = Math.PI / 180;
const SUNRISE_ELEVATION = -0.833; // Standard refraction and the Sun's apparent radius.

export function daylightAt(date = new Date()) {
  const year = date.getUTCFullYear();
  const yearStart = Date.UTC(year, 0, 1);
  const daysInYear = (Date.UTC(year + 1, 0, 1) - yearStart) / 86400000;
  const day = Math.floor((date.getTime() - yearStart) / 86400000);
  const hour =
    date.getUTCHours() +
    date.getUTCMinutes() / 60 +
    date.getUTCSeconds() / 3600;
  const gamma = ((2 * Math.PI) / daysInYear) * (day + (hour - 12) / 24);
  const equationOfTime =
    229.18 *
    (0.000075 +
      0.001868 * Math.cos(gamma) -
      0.032077 * Math.sin(gamma) -
      0.014615 * Math.cos(2 * gamma) -
      0.040849 * Math.sin(2 * gamma));
  const declination =
    0.006918 -
    0.399912 * Math.cos(gamma) +
    0.070257 * Math.sin(gamma) -
    0.006758 * Math.cos(2 * gamma) +
    0.000907 * Math.sin(2 * gamma) -
    0.002697 * Math.cos(3 * gamma) +
    0.00148 * Math.sin(3 * gamma);
  const solarMinutes = hour * 60 + equationOfTime + 4 * LONGYEARBYEN.longitude;
  const hourAngle = (solarMinutes / 4 - 180) * RADIANS;
  const latitude = LONGYEARBYEN.latitude * RADIANS;
  const center = Math.sin(latitude) * Math.sin(declination);
  const swing = Math.cos(latitude) * Math.cos(declination);
  const degrees = (value) =>
    Math.asin(Math.max(-1, Math.min(1, value))) / RADIANS;
  const elevation = degrees(center + swing * Math.cos(hourAngle));
  const maximum = degrees(center + swing);
  const minimum = degrees(center - swing);
  const isDay = elevation >= SUNRISE_ELEVATION;
  const phase = isDay
    ? minimum >= SUNRISE_ELEVATION
      ? "Midnight sun"
      : "Daylight"
    : elevation >= -6
      ? "Twilight"
      : maximum < -6
        ? "Polar night"
        : "Night";
  return { elevation, phase, theme: isDay ? "light" : "dark" };
}
