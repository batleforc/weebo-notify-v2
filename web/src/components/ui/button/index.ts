import { cva, type VariantProps } from "class-variance-authority";

export { default as Button } from "./Button.vue";

/**
 * shadcn's button, vendored (ADR-0004).
 *
 * Every colour here is a semantic token — no `bg-blue-500` anywhere — which is the
 * one blocking rule of a front-end review (§10.6): it is what makes the
 * `monofolio` swap a token file rather than a fork.
 */
export const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-[color,background-color,box-shadow,transform] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:pointer-events-none disabled:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        // The one variant the design singles out for the brand's glow (§10.6): a
        // primary action is the thing on a screen most worth drawing the eye to,
        // and `shadow-md` now resolves to `--elevation-md` (theme.css) rather than
        // Tailwind's stock grey — a real glow in dark, a firm shadow in light. The
        // 1px hover lift + baseline-on-press pairing mirrors the source system's
        // own primary-button spec.
        default:
          "bg-primary text-primary-foreground shadow-md hover:bg-primary/90 hover:-translate-y-px active:translate-y-0",
        destructive:
          "bg-destructive text-destructive-foreground hover:bg-destructive/90",
        outline:
          "border border-border bg-background hover:bg-accent hover:text-accent-foreground",
        secondary:
          "bg-secondary text-secondary-foreground hover:bg-secondary/80",
        ghost: "hover:bg-accent hover:text-accent-foreground",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default: "h-9 px-4 py-2",
        sm: "h-8 rounded-md px-3 text-xs",
        lg: "h-10 rounded-md px-8",
        icon: "size-9",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

export type ButtonVariants = VariantProps<typeof buttonVariants>;
