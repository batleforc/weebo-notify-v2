import { cva, type VariantProps } from "class-variance-authority";

export { default as Badge } from "./Badge.vue";

/** A state chip: square, 1px, uppercase tracked meta. */
export const badgeVariants = cva(
  "inline-flex items-center gap-1 border px-1.5 py-px text-[0.6875rem] font-medium uppercase leading-4 tracking-[0.08em] tabular-nums",
  {
    variants: {
      variant: {
        default: "border-signal text-signal",
        secondary: "border-rule-strong bg-ground-raised text-ink",
        outline: "border-rule-strong text-ink-dim",
        muted: "border-rule text-ink-dim normal-case tracking-normal",
      },
    },
    defaultVariants: { variant: "outline" },
  },
);

export type BadgeVariants = VariantProps<typeof badgeVariants>;
