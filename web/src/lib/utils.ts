import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

/** Merge conditional class lists, letting the last utility of a family win. */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
