import { cva, type VariantProps } from "class-variance-authority";

export { default as Button } from "./Button.vue";

/** shadcn's button, vendored (ADR-0004), redrawn in the proof-sheet grammar:
 * square, 1px rule, uppercase tracked mono; crimson fill only on the one action. */
export const buttonVariants = cva(
  "inline-flex shrink-0 select-none items-center justify-center gap-2 whitespace-nowrap border text-xs font-medium uppercase tracking-[0.08em] transition-colors disabled:pointer-events-none disabled:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default:
          "border-primary bg-primary text-primary-foreground disabled:border-rule-strong disabled:bg-transparent disabled:text-ink-dim disabled:opacity-100 hover:shadow-[0_0_0_2px_var(--ground),0_0_0_3px_var(--signal)] active:translate-x-[-2px] active:shadow-[3px_0_0_var(--signal-ink),6px_0_0_var(--signal)]",
        destructive:
          "border-signal text-signal hover:bg-signal hover:text-primary-foreground",
        outline:
          "border-rule-strong bg-transparent text-ink-dim hover:border-ink-dim hover:text-ink aria-pressed:border-copper aria-pressed:text-ink",
        secondary:
          "border-rule-strong bg-ground-raised text-ink hover:border-ink-dim",
        ghost:
          "border-transparent text-ink-dim hover:bg-ground-raised hover:text-ink",
        link: "border-transparent normal-case tracking-normal text-signal underline-offset-4 hover:underline",
      },
      size: {
        default: "h-9 px-4",
        sm: "h-8 px-3",
        lg: "h-11 px-6",
        icon: "size-9 pointer-coarse:size-11",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

export type ButtonVariants = VariantProps<typeof buttonVariants>;
