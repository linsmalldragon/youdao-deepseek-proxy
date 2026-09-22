use std::collections::BTreeMap;

use md5::Context;

/// Compute the MD5 hex digest of a string (md5 crate 0.7's `Context` API).
pub fn md5_hex(s: &str) -> String {
    let mut c = Context::new();
    c.consume(s.as_bytes());
    let d: [u8; 16] = c.compute().into();
    hex(&d)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Faithful port of the Youdao app's module-30933 `genSign`.
///
/// Mirrors the JS exactly:
/// ```js
/// o = (params, secretKey) => {
///   const o = {...params};
///   Object.keys(o).forEach(k => { if (o[k] === "") delete o[k]; });   // drop empty
///   const keys = Object.keys(o).sort().filter(k => o[k] !== undefined);
///   keys.push("key"); o["key"] = secretKey;
///   const str = keys.map(k => `${k}=${o[k]}`).join("&");
///   return [MD5(str), keys.join(",")];
/// }
/// ```
/// Returns `(sign, point_param)` where `point_param` is the sorted key list
/// (with the trailing `key`), comma-joined.
pub fn gen_sign(params: &BTreeMap<String, String>, secret_key: &str) -> (String, String) {
    // Drop empty-string values before signing (the `""` entries are removed).
    let o: BTreeMap<String, String> = params
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // BTreeMap keeps keys sorted lexicographically, matching JS `Object.keys(o).sort()`.
    let mut keys: Vec<String> = o.keys().cloned().collect();
    keys.push("key".to_string());

    let signed = keys
        .iter()
        .map(|k| {
            let v = if k == "key" {
                secret_key
            } else {
                o.get(k).expect("key present in map").as_str()
            };
            format!("{}={}", k, v)
        })
        .collect::<Vec<_>>()
        .join("&");
    let sign = md5_hex(&signed);
    let point_param = keys.join(",");
    (sign, point_param)
}

/// Sign a `biz` field set (30933 `genParamV3`): the caller passes the full
/// merged field map; we add `sign` + `pointParam`. Empty-string values are
/// dropped by the signer, matching the app's `FormData` skip-empty behavior.
pub fn sign_biz(biz: &BTreeMap<String, String>, secret_key: &str) -> BTreeMap<String, String> {
    let (sign, point_param) = gen_sign(biz, secret_key);
    let mut out = biz.clone();
    out.insert("sign".into(), sign);
    out.insert("pointParam".into(), point_param);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_dropped_and_key_appended() {
        let mut p = BTreeMap::new();
        p.insert("a".into(), "1".into());
        p.insert("b".into(), String::new()); // empty -> dropped
        p.insert("c".into(), "3".into());
        let (sign, pp) = gen_sign(&p, "SECRET");
        assert_eq!(pp, "a,c,key");
        let expect = md5_hex("a=1&c=3&key=SECRET");
        assert_eq!(sign, expect);
    }

    #[test]
    fn known_md5() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }
}
