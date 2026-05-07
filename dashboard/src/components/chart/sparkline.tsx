import {
  AreaSeries,
  ColorType,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type LineData,
  type UTCTimestamp,
} from "lightweight-charts";
import { useEffect, useRef } from "react";

import type { TimePoint } from "@/hooks/use-time-series";

interface Props {
  data: TimePoint[];
  /** RGB hex tint for line + fill. Defaults to the theme primary. */
  tone?: "primary" | "emerald" | "amber" | "red";
  height?: number;
}

const TONE_COLORS: Record<NonNullable<Props["tone"]>, string> = {
  primary: "#38bdf8",
  emerald: "#34d399",
  amber: "#fbbf24",
  red: "#f87171",
};

export function Sparkline({ data, tone = "primary", height = 56 }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Area"> | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;
    const tint = TONE_COLORS[tone];

    const chart = createChart(containerRef.current, {
      width: containerRef.current.clientWidth,
      height,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: "rgba(148, 163, 184, 0.6)",
        attributionLogo: false,
      },
      grid: {
        horzLines: { visible: false },
        vertLines: { visible: false },
      },
      rightPriceScale: { visible: false },
      leftPriceScale: { visible: false },
      timeScale: { visible: false },
      crosshair: { vertLine: { visible: false }, horzLine: { visible: false } },
      handleScroll: false,
      handleScale: false,
    });

    const series = chart.addSeries(AreaSeries, {
      lineColor: tint,
      topColor: `${tint}55`,
      bottomColor: `${tint}00`,
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: false,
    });

    chartRef.current = chart;
    seriesRef.current = series;

    const ro = new ResizeObserver((entries) => {
      for (const e of entries) {
        chart.applyOptions({ width: e.contentRect.width });
      }
    });
    ro.observe(containerRef.current);

    return () => {
      ro.disconnect();
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, [tone, height]);

  useEffect(() => {
    const series = seriesRef.current;
    const chart = chartRef.current;
    if (!series || !chart) return;
    const points: LineData<UTCTimestamp>[] = data.map((p) => ({
      time: p.time as UTCTimestamp,
      value: p.value,
    }));
    series.setData(points);
    chart.timeScale().fitContent();
  }, [data]);

  return <div ref={containerRef} className="w-full" style={{ height }} />;
}
