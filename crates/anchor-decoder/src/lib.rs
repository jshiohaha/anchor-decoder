extern crate proc_macro;
use std::collections::HashSet;

use proc_macro::TokenStream;
use quote::quote;
use serde_json::Value;
use syn::{parse_macro_input, LitStr};

/// Helper to convert snake_case to CamelCase (e.g. "create_order" -> "CreateOrder")
fn to_camel_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut c = word.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

/// Maps an IDL type (which can be a string like "u8" or an object for arrays or defined types)
/// into the corresponding Rust type as tokens. The `generated_types` set contains the names
/// of custom types that will be generated by this macro.
fn map_idl_type(arg_type: &Value, generated_types: &HashSet<String>) -> proc_macro2::TokenStream {
    if let Some(s) = arg_type.as_str() {
        match s {
            "u8" => quote! { u8 },
            "u16" => quote! { u16 },
            "u64" => quote! { u64 },
            "i64" => quote! { i64 },
            "bool" => quote! { bool },
            "pubkey" => quote! { Pubkey },
            "string" => quote! { String },
            _ => quote! { () }, // fallback for unsupported types
        }
    } else if let Some(obj) = arg_type.as_object() {
        if let Some(array_val) = obj.get("array") {
            if let Some(arr) = array_val.as_array() {
                if arr.len() == 2 {
                    let inner = map_idl_type(&arr[0], generated_types);
                    if let Some(len) = arr[1].as_u64() {
                        let len_literal =
                            syn::LitInt::new(&len.to_string(), proc_macro2::Span::call_site());
                        return quote! { [#inner; #len_literal] };
                    }
                }
            }
        } else if let Some(defined) = obj.get("defined") {
            if let Some(defined_obj) = defined.as_object() {
                if let Some(name) = defined_obj.get("name").and_then(|n| n.as_str()) {
                    let type_ident = syn::Ident::new(name, proc_macro2::Span::call_site());
                    // If the type was generated by our macro, reference it directly.
                    // Otherwise assume it's external (and qualify it).
                    if generated_types.contains(name) {
                        return quote! { #type_ident };
                    } else {
                        return quote! { ::crate::#type_ident };
                    }
                }
            }
        }
        quote! { () }
    } else {
        quote! { () }
    }
}

/// Procedural macro attribute that generates decoding code from an Anchor IDL JSON file.
/// The macro reads the file at compile time
/// 
/// For each instruction:
///  - It generates a struct for the instruction's arguments (if any), with a constant discriminator.
///  - It creates an enum variant for the instruction.
///  - It produces a helper function (`decode_instruction`) to match and decode incoming data.
///
/// For each account:
///  - It assumes the account type is defined under "types" (by matching name).
///  - It uses the provided discriminator to generate a match arm that decodes the account data,
///    skipping the first 8 bytes.
#[proc_macro_attribute]
pub fn anchor_idl(attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Get the relative IDL file path from the attribute
    let relative_path = parse_macro_input!(attr as LitStr).value();

    // Resolve path relative to crate root
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR environment variable not set");
    let idl_path = std::path::Path::new(&manifest_dir)
        .join(relative_path)
        .canonicalize()
        .unwrap_or_else(|e| panic!("Failed to resolve IDL path: {}", e));

    // Read and parse the IDL JSON at compile time
    let idl_json = std::fs::read_to_string(&idl_path)
        .unwrap_or_else(|_| panic!("Unable to read IDL file at: {}", idl_path.display()));
    let idl: Value = serde_json::from_str(&idl_json)
        .unwrap_or_else(|_| panic!("Invalid JSON in IDL file: {}", idl_path.display()));

    // Collect the names of all types defined in the IDL.
    let generated_types: HashSet<String> =
        if let Some(types) = idl.get("types").and_then(|v| v.as_array()) {
            types
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        } else {
            HashSet::new()
        };

    let mut struct_defs = Vec::new();

    // Process custom type definitions.
    if let Some(types) = idl.get("types").and_then(|v| v.as_array()) {
        for type_def in types {
            if let (Some(name), Some(type_info)) = (
                type_def.get("name").and_then(|v| v.as_str()),
                type_def.get("type").and_then(|v| v.as_object()),
            ) {
                let type_ident = syn::Ident::new(name, proc_macro2::Span::call_site());

                // Check the kind of the type.
                if let Some(kind) = type_info.get("kind").and_then(|v| v.as_str()) {
                    match kind {
                        "struct" => {
                            // Process struct definitions.
                            let field_defs = if let Some(fields) = type_info.get("fields").and_then(|v| v.as_array()) {
                                let mut field_defs = Vec::new();
                                for field in fields {
                                    if let (Some(field_name), Some(field_type)) = (
                                        field.get("name").and_then(|v| v.as_str()),
                                        field.get("type"),
                                    ) {
                                        let field_ident = syn::Ident::new(
                                            field_name,
                                            proc_macro2::Span::call_site(),
                                        );
                                        let field_type = map_idl_type(field_type, &generated_types);
                                        field_defs.push(quote! {
                                            pub #field_ident: #field_type,
                                        });
                                    }
                                }
                                field_defs
                            } else {
                                // Handle empty struct (no fields property)
                                Vec::new()
                            };
                            
                            struct_defs.push(quote! {
                                #[derive(Debug, BorshSerialize, BorshDeserialize)]
                                pub struct #type_ident {
                                    #( #field_defs )*
                                }
                                impl #type_ident {
                                    pub fn decode(data: &[u8]) -> Result<Self, ::std::io::Error> {
                                        <Self as BorshDeserialize>::try_from_slice(data)
                                    }
                                }
                            });
                        }
                        "enum" => {
                            // Process enum definitions.
                            if let Some(variants) =
                                type_info.get("variants").and_then(|v| v.as_array())
                            {
                                let mut variant_tokens = Vec::new();
                                for variant in variants {
                                    if let Some(variant_name) =
                                        variant.get("name").and_then(|v| v.as_str())
                                    {
                                        let variant_ident = syn::Ident::new(
                                            variant_name,
                                            proc_macro2::Span::call_site(),
                                        );
                                        variant_tokens.push(quote! {
                                            #variant_ident,
                                        });
                                    }
                                }
                                struct_defs.push(quote! {
                                    #[derive(Debug, BorshSerialize, BorshDeserialize)]
                                    pub enum #type_ident {
                                        #( #variant_tokens )*
                                    }
                                    impl #type_ident {
                                        pub fn decode(data: &[u8]) -> Result<Self, ::std::io::Error> {
                                            <Self as BorshDeserialize>::try_from_slice(data)
                                        }
                                    }
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let instructions = idl
        .get("instructions")
        .and_then(|v| v.as_array())
        .expect("IDL JSON does not contain an 'instructions' array");

    let mut enum_variants = Vec::new();
    let mut match_arms = Vec::new();

    for inst in instructions {
        // Get instruction name, discriminator, and args.
        let name = inst.get("name").and_then(|v| v.as_str()).unwrap();
        let discriminator = inst
            .get("discriminator")
            .and_then(|v| v.as_array())
            .expect("Discriminator missing or not an array");
        let args = inst
            .get("args")
            .and_then(|v| v.as_array())
            .expect("Args missing or not an array");

        // Convert the instruction name to CamelCase for the generated struct.
        let struct_name_str = to_camel_case(name);
        let struct_name = syn::Ident::new(&struct_name_str, proc_macro2::Span::call_site());

        // Generate account info struct name
        let accounts_struct_name = syn::Ident::new(
            &format!("{}Accounts", struct_name_str),
            proc_macro2::Span::call_site(),
        );

        // Generate a constant for the discriminator.
        let disc_values: Vec<u8> = discriminator
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect();
        let disc_tokens = quote! { [ #( #disc_values ),* ] };

        // Process accounts for this instruction
        let mut account_consts = Vec::new();
        let mut account_fields = Vec::new();
        let mut account_indices = Vec::new();
        let mut account_name_matches = Vec::new();
        let mut account_tuples = Vec::new();
        let mut account_index_matches = Vec::new();

        if let Some(accounts) = inst.get("accounts").and_then(|v| v.as_array()) {
            for (idx, account) in accounts.iter().enumerate() {
                if let Some(account_name) = account.get("name").and_then(|v| v.as_str()) {
                    let const_name = account_name.to_uppercase();
                    let const_ident = syn::Ident::new(&const_name, proc_macro2::Span::call_site());
                    let idx_lit =
                        syn::LitInt::new(&idx.to_string(), proc_macro2::Span::call_site());

                    account_consts.push(quote! {
                        pub const #const_ident: usize = #idx_lit;
                    });

                    let field_ident = syn::Ident::new(account_name, proc_macro2::Span::call_site());
                    account_fields.push(quote! {
                        pub #field_ident: usize,
                    });

                    account_indices.push(quote! {
                        #field_ident: #idx_lit,
                    });

                    // Create match arm for get_account_name
                    let account_name_str = account_name;
                    account_name_matches.push(quote! {
                        #idx_lit => Some(#account_name_str),
                    });

                    // Create tuple for get_all_accounts
                    account_tuples.push(quote! {
                        (#account_name_str, Self::#const_ident)
                    });

                    // Create match arm for get_account_index
                    account_index_matches.push(quote! {
                        #account_name_str => Some(Self::#const_ident),
                    });
                }
            }

            // Generate the accounts struct
            struct_defs.push(quote! {
                #[derive(Debug, Clone, Copy)]
                pub struct #accounts_struct_name {
                    #( #account_fields )*
                }

                impl #accounts_struct_name {
                    #( #account_consts )*

                    pub const fn new() -> Self {
                        Self {
                            #( #account_indices )*
                        }
                    }

                    pub fn get_account_name(&self, index: usize) -> Option<&'static str> {
                        match index {
                            #( #account_name_matches )*
                            _ => None,
                        }
                    }

                    pub fn get_all_accounts(&self) -> &'static [(&'static str, usize)] {
                        &[
                            #( #account_tuples, )*
                        ]
                    }
                    
                    pub fn get_account_index(&self, name: &str) -> Option<usize> {
                        match name {
                            #( #account_index_matches )*
                            _ => None,
                        }
                    }
                }
            });
        }

        if !args.is_empty() {
            // Generate struct fields by mapping each argument's type.
            let mut fields = Vec::new();
            for arg in args {
                let arg_name = arg.get("name").and_then(|v| v.as_str()).unwrap();
                let arg_type = arg.get("type").expect("Missing type in argument");
                let field_ident = syn::Ident::new(arg_name, proc_macro2::Span::call_site());
                let field_type = map_idl_type(arg_type, &generated_types);
                fields.push(quote! {
                    pub #field_ident: #field_type,
                });
            }

            struct_defs.push(quote! {
                #[derive(Debug, BorshSerialize, BorshDeserialize)]
                pub struct #struct_name {
                    #( #fields )*
                }
                impl #struct_name {
                    pub const DISCRIMINATOR: [u8; 8] = #disc_tokens;
                    pub const ACCOUNTS: #accounts_struct_name = #accounts_struct_name::new();
                    
                    pub fn decode(data: &[u8]) -> Result<Self, ::std::io::Error> {
                        // Skip the first 8 bytes (discriminator)
                        let payload = &data[8..];
                        <Self as BorshDeserialize>::try_from_slice(payload)
                    }
                    
                    /// Maps account indices to their semantic names
                    pub fn map_accounts<'a>(accounts: &'a [Pubkey]) -> std::collections::HashMap<&'static str, &'a Pubkey> {
                        let mut result = std::collections::HashMap::new();
                        for (i, account) in accounts.iter().enumerate() {
                            if let Some(name) = Self::ACCOUNTS.get_account_name(i) {
                                result.insert(name, account);
                            }
                        }
                        result
                    }
                }
            });

            enum_variants.push(quote! {
                #struct_name(#struct_name)
            });
            match_arms.push(quote! {
                x if x == #struct_name::DISCRIMINATOR => {
                    return Some(DecodedInstruction::#struct_name(
                        #struct_name::decode(data).ok()?
                    ))
                }
            });
        } else {
            // For instructions with no arguments, generate a unit struct.
            struct_defs.push(quote! {
                #[derive(Debug)]
                pub struct #struct_name;
                impl #struct_name {
                    pub const DISCRIMINATOR: [u8; 8] = #disc_tokens;
                    pub const ACCOUNTS: #accounts_struct_name = #accounts_struct_name::new();
                    
                    /// Maps account indices to their semantic names
                    pub fn map_accounts<'a>(accounts: &'a [Pubkey]) -> std::collections::HashMap<&'static str, &'a Pubkey> {
                        let mut result = std::collections::HashMap::new();
                        for (i, account) in accounts.iter().enumerate() {
                            if let Some(name) = Self::ACCOUNTS.get_account_name(i) {
                                result.insert(name, account);
                            }
                        }
                        result
                    }
                }
            });
            enum_variants.push(quote! {
                #struct_name
            });
            match_arms.push(quote! {
                x if x == #struct_name::DISCRIMINATOR => {
                    return Some(DecodedInstruction::#struct_name)
                }
            });
        }
    }

    // Process accounts from the IDL.
    let mut account_enum_variants = Vec::new();
    let mut account_match_arms = Vec::new();
    if let Some(accounts) = idl.get("accounts").and_then(|v| v.as_array()) {
        for account in accounts {
            let name = account.get("name").and_then(|v| v.as_str()).unwrap();
            let discriminator = account
                .get("discriminator")
                .and_then(|v| v.as_array())
                .expect("Discriminator missing or not an array in accounts");
            let type_ident = syn::Ident::new(name, proc_macro2::Span::call_site());
            let disc_values: Vec<u8> = discriminator
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            let disc_tokens = quote! { [ #( #disc_values ),* ] };

            account_enum_variants.push(quote! {
                #type_ident(#type_ident)
            });
            account_match_arms.push(quote! {
                x if x == #disc_tokens => {
                    return Some(DecodedAccount::#type_ident(
                        #type_ident::decode(&data[8..]).ok()?
                    ))
                }
            });
        }
    }

    // Process events from the IDL.
    let mut event_enum_variants = Vec::new();
    let mut event_match_arms = Vec::new();
    if let Some(events) = idl.get("events").and_then(|v| v.as_array()) {
        for event in events {
            let name = event.get("name").and_then(|v| v.as_str()).unwrap();
            let discriminator = event
                .get("discriminator")
                .and_then(|v| v.as_array())
                .expect("Discriminator missing or not an array in events");
            let type_ident = syn::Ident::new(name, proc_macro2::Span::call_site());
            let disc_values: Vec<u8> = discriminator
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            let disc_tokens = quote! { [ #( #disc_values ),* ] };

            event_enum_variants.push(quote! {
                #type_ident(#type_ident)
            });
            event_match_arms.push(quote! {
                x if x == #disc_tokens => {
                    return Some(DecodedEvent::#type_ident(
                        #type_ident::decode(&data[8..]).ok()?
                    ))
                }
            });
        }
    }

    let program_address = idl
        .get("address")
        .and_then(|v| v.as_str())
        .expect("IDL missing program address");

    let expanded = quote! {
        use ::borsh::{BorshDeserialize, BorshSerialize};
        use ::solana_sdk::pubkey::Pubkey;
        use std::collections::HashMap;

        pub const ID: Pubkey = ::solana_sdk::pubkey!(#program_address);

        #( #struct_defs )*

        #[derive(Debug)]
        pub enum DecodedInstruction {
            #( #enum_variants, )*
            EmitCpi(DecodedEvent)
        }

        pub fn decode_instruction(data: &[u8]) -> Option<DecodedInstruction> {
            if data.len() < 8 { return None; }
            let disc = &data[..8];
            match disc {
                #( #match_arms, )*
                _ => {
                    if disc == EMIT_CPI_INSTRUCTION_DISCRIMINATOR {
                        let payload = &data[8..];
                        decode_event(payload).map(|event| DecodedInstruction::EmitCpi(event))
                    } else {
                        None
                    }
                },
            }
        }

        #[derive(Debug)]
        pub enum DecodedAccount {
            #( #account_enum_variants, )*
        }

        pub fn decode_account(data: &[u8]) -> Option<DecodedAccount> {
            if data.len() < 8 { return None; }
            let disc = &data[..8];
            match disc {
                #( #account_match_arms, )*
                _ => {
                    None
                },
            }
        }

        #[derive(Debug)]
        pub enum DecodedEvent {
            #( #event_enum_variants, )*
        }

        // Some programs might call anchor's emit_cpi instruction to emit events via self-cpi so that subscribed clients
        // can see the events without risk of the RPC's truncating them (as with traditional event logging)
        //
        // Source: https://github.com/coral-xyz/anchor/blob/8b391aa278387b6f6ce3133453619a175544631e/lang/attribute/event/src/lib.rs#L111-L195
        const EMIT_CPI_INSTRUCTION_DISCRIMINATOR: [u8; 8] = [228, 69, 165, 46, 81, 203, 154, 29];

        pub fn decode_event(data: &[u8]) -> Option<DecodedEvent> {
            if data.len() < 8 { return None; }
            let disc = &data[..8];

            match disc {
                #( #event_match_arms, )*
                _ => {
                    None
                }
            }
        }
    };

    expanded.into()
}
