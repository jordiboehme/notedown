// The account name (slug) a new account gets from its e-mail. It is shown in an editable field.

/** The local part of the e-mail, lower case; every character outside `a-z 0-9 - _` becomes
 * `-`, runs of `-` collapse into one, `-` and `_` are trimmed from both ends, then it is cut to
 * 64 characters (and trimmed again at the cut). `me` when nothing is left. */
export function slugFromEmail(email: string): string {
  const at = email.indexOf("@");
  const local = at === -1 ? email : email.slice(0, at);
  const slug = local
    .toLowerCase()
    .replace(/[^a-z0-9_-]/g, "-")
    .replace(/-+/g, "-")
    .replace(/^[-_]+|[-_]+$/g, "")
    .slice(0, 64)
    .replace(/[-_]+$/, "");
  return slug === "" ? "me" : slug;
}
