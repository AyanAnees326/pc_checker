import { useEffect, useRef } from "react";
import { motion, useMotionValue, useReducedMotion, useTransform, animate } from "framer-motion";

const defaultFormat = (v: number) => (Number.isInteger(v) ? String(v) : v.toFixed(1));

/**
 * Tweens the displayed text from the previous numeric value to the next one whenever
 * `value` changes, instead of the number just snapping — the one small "flashy"
 * numeric effect reused everywhere a live metric updates (`Stat`, `MetricGraph`),
 * rather than several bespoke ones. Falls back to an instant swap under
 * `prefers-reduced-motion`.
 */
export function AnimatedCounter({
  value,
  format,
  suffix,
  className,
}: {
  value: number;
  format?: (v: number) => string;
  suffix?: string;
  className?: string;
}) {
  const reducedMotion = useReducedMotion();
  const motionValue = useMotionValue(value);
  const rendered = useTransform(motionValue, (v) => `${(format ?? defaultFormat)(v)}${suffix ? ` ${suffix}` : ""}`);
  const mounted = useRef(false);

  useEffect(() => {
    if (!mounted.current || reducedMotion) {
      motionValue.set(value);
      mounted.current = true;
      return;
    }
    const controls = animate(motionValue, value, { duration: 0.5, ease: "easeOut" });
    return () => controls.stop();
  }, [value, reducedMotion, motionValue]);

  return <motion.span className={className}>{rendered}</motion.span>;
}
