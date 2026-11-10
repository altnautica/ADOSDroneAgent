import { useEffect, useState } from "react";

/** Wall-clock now (the clock react-query stamps `dataUpdatedAt` with), re-read every `ms`. */
export function useNow(ms: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(id);
  }, [ms]);
  return now;
}
