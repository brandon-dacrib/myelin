import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

/** Merge Tailwind class lists, resolving conflicting utilities left-to-right-losing. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
