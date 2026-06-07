use std::env;
use std::fs;
use sigil_tx::{ed25519_keygen, ed25519_sign_tx, wallet_id_from_pubkey, SigilTx, SignedTx};

fn parse_u128(v: &serde_json::Value) -> u128 {
    match v {
        serde_json::Value::String(s) => s.parse().unwrap_or(0),
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as u128,
        _ => 0,
    }
}

fn main() {
    let registry_path = env::args().nth(1).expect("registry.json");
    let use_deployer = env::args().nth(2).unwrap_or_else(|| "generate".into());
    let (sk, pk, wallet) = if use_deployer == "generate" {
        ed25519_keygen()
    } else {
        let sk_hex = use_deployer;
        let pk_hex = env::args().nth(3).expect("pk_hex when using fixed sk");
        let mut sk = [0u8; 32]; let mut pk = [0u8; 32];
        for i in 0..32 {
            sk[i] = u8::from_str_radix(&sk_hex[i*2..i*2+2], 16).unwrap();
            pk[i] = u8::from_str_radix(&pk_hex[i*2..i*2+2], 16).unwrap();
        }
        (sk, pk, wallet_id_from_pubkey(&pk))
    };
    eprintln!("deployer_wallet={}", wallet.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let raw = fs::read_to_string(&registry_path).expect("read registry");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse");
    let mut signed: Vec<SignedTx> = Vec::new();
    for txv in doc["txs_preview"].as_array().unwrap() {
        let kind = txv["kind"].as_str().unwrap();
        let tx = match kind {
            "TokenDeploy" => SigilTx::TokenDeploy {
                creator: wallet,
                ticker: txv["ticker"].as_str().unwrap().into(),
                decimals: txv["decimals"].as_u64().unwrap() as u8,
                initial_supply: parse_u128(&txv["initial_supply"]),
                fee: parse_u128(&txv["fee"]),
            },
            "ContractDeploy" => {
                let bytecode: Vec<u8> = txv["bytecode"].as_array().unwrap()
                    .iter().map(|v| v.as_u64().unwrap() as u8).collect();
                SigilTx::ContractDeploy {
                    from: wallet,
                    bytecode,
                    constructor_args: vec![],
                    gas_limit: txv["gas_limit"].as_u64().unwrap(),
                    fee: parse_u128(&txv["fee"]),
                }
            }
            _ => panic!("unknown kind"),
        };
        signed.push(ed25519_sign_tx(tx, &sk, &pk));
    }
    println!("{}", serde_json::to_string_pretty(&signed).unwrap());
}
