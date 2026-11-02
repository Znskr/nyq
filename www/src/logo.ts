/** The logo grid, row by row (1 = filled). See Logo.astro. */
export const GRID = [
  [1, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1],
  [1, 0, 1, 0, 1, 1, 1, 0, 1, 0, 1],
  [1, 0, 1, 0, 0, 0, 1, 0, 1, 1, 1],
  [0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 1],
];

/** The filled cells as [x, y]. */
export const PIXELS: [number, number][] = GRID.flatMap((row, y) =>
  row.flatMap((on, x) => (on ? [[x, y] as [number, number]] : [])),
);
