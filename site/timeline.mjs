// Seconds within one illustrated run. Later events derive from earlier ones,
// so retiming the read cannot let the upload overlap the open snapshot.
export const CYCLE = 24;
export const FIRE = 2.2; // The schedule starts svalbard.
export const SPARK = 1.1; // The start signal reaches svalbard.
export const OPEN = 3.7; // The database snapshot opens.
export const READ = [4.1, 8.7]; // Data leaves the database.
export const GATHER = [0.9, 1.4]; // Time spent gathering above the database.
export const CARRY = [2.3, 3.1]; // Time spent travelling to svalbard.
export const LAST_READ = READ[1] + GATHER[1];
export const CLOSE = LAST_READ + 0.3;
export const WRITTEN = LAST_READ + CARRY[1];
const uploadStart = Math.max(CLOSE, WRITTEN) + 0.5;
export const UPLOAD = [uploadStart, uploadStart + 3];
export const FLIGHT = 1.5;
export const FALL = 0.9;
export const STORED = UPLOAD[1] + FLIGHT + FALL;
export const COOL = [STORED + 0.4, STORED + 2.4];
export const SINK = [STORED + 1.2, STORED + 2.6];

export function phaseAt(t) {
  return {
    started: t >= FIRE,
    snapshot: t < OPEN ? "" : t < CLOSE ? "open" : "closed",
    svalbard:
      t < FIRE
        ? ""
        : t < UPLOAD[0]
          ? "running"
          : t < STORED
            ? "uploading"
            : "finished",
    storage: t < UPLOAD[0] ? "" : t < STORED ? "receiving" : "stored",
  };
}
