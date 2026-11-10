// The owner every vision lease from this page carries: one per page load, so
// two dashboard tabs never drop each other's lease.

export const LEASE_OWNER = `dashboard:${Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0")}`;
