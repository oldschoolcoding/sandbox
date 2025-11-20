import yaml
import random
import string
import json
import re 
from typing import Dict, Any, List, Tuple
from sqlalchemy import create_engine, Column, String, JSON, Integer, Text, ForeignKey, PrimaryKeyConstraint, ForeignKeyConstraint, text, select
from sqlalchemy.orm import sessionmaker, relationship
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.exc import OperationalError, ProgrammingError
from collections import defaultdict, OrderedDict 
from datetime import datetime 
import tkinter as tk
from tkinter import ttk, scrolledtext
from tkinter.messagebox import showerror

# --- Configuration Globale ----
NB_APPLICATIONS = 20
NB_MODULES_PER_APP = 20
OUTPUT_FILENAME = "generated_app_configs.txt" 
CYBERARK_FILENAME = "cyberark-configs.csv" 
KEY_PREFIX = "APP" 

# Variables de connexion MISES À JOUR
#DB_URL_TEMPLATE = "postgresql://localhost:5432/{}" 
DB_URL_TEMPLATE = "postgresql:///{}" 
DB_NAME = "config_db" 
# Chaîne de connexion UNIQUE utilisée partout pour la DB spécifique
DB_URL = DB_URL_TEMPLATE.format(DB_NAME) 

# NOUVEAU MAPPING : Identifiant arbitraire pour complexifier la clé
ARBITRARY_MAPPINGS = [
    'primary', 'secondary', 'logs', 'audit', 'reporting', 'ro', 'rw'
]

Base = declarative_base()

# Mapping des préfixes de serveurs par environnement
ENVIRONMENT_PREFIXES = {
    'dev': 'vd',
    'tst': 'vt',
    'acc': 'va',
    'prd': 'vp',
}
ENVIRONMENTS = list(ENVIRONMENT_PREFIXES.keys())

# Nouveaux environnements groupés pour CyberArk
CYBERARK_ENV_MAP = {
    'dev': 'DEV_TST',
    'tst': 'DEV_TST',
    'acc': 'ACC_PRD',
    'prd': 'ACC_PRD',
}

# Types de bases de données et leurs caractéristiques
DATABASE_TYPES = {
    'oracle': {
        'driver': 'oracle.jdbc.OracleDriver',
        'conn_prefix': 'jdbc:oracle:thin:@',
        'ports': [1521, 1522]
    },
    'cassandra': {
        'driver': 'com.datastax.driver.core.Cluster',
        'conn_prefix': 'cassandra://',
        'ports': [9042]
    },
    'db2': {
        'driver': 'com.ibm.db2.jcc.DB2Driver',
        'conn_prefix': 'jdbc:db2://',
        'ports': [50000, 50001]
    }
}
DB_NAMES = list(DATABASE_TYPES.keys())

# --- TABLE DE MAPPING REGEX ---
DB_MAPPING_REGEX = {
    'oracle': r".*/([a-zA-Z0-9_-]+)$", 
    'db2': r".*/([a-zA-Z0-9_-]+)$",
    'cassandra': r".*/([a-zA-Z0-9_-]+)$", 
}

def get_database_name_from_url(db_type: str, url: str) -> str:
    """Applique la regex de mapping pour trouver le nom de la base de données logique."""
    if db_type not in DB_MAPPING_REGEX:
        return "UNKNOWN_DB_NAME" 

    regex = DB_MAPPING_REGEX[db_type]
    
    match = re.search(regex, url)
    
    if match and len(match.groups()) > 0:
        return match.group(1).upper()
    
    return "UNKNOWN_DB_NAME"


# --- Modèles PostgreSQL (SQLAlchemy) ---

class ApplicationConfig(Base):
    """Table 1: Configuration complète (Source de vérité)"""
    __tablename__ = 'java_configs'
    
    app_name = Column(String)
    mdl_name = Column(String)
    environment = Column(String) 
    
    full_config = Column(JSON) 
    
    __table_args__ = (
        PrimaryKeyConstraint('app_name', 'mdl_name', 'environment'),
    )
    
    properties = relationship("ApplicationProperty", back_populates="config", cascade="all, delete-orphan")

class ApplicationProperty(Base):
    """Table 2: Propriétés dénormalisées pour le filtrage global."""
    __tablename__ = 'app_properties'
    
    id = Column(Integer, primary_key=True, autoincrement=True)
    
    app_name = Column(String)
    mdl_name = Column(String)
    environment = Column(String) 
    
    prop_key = Column(String)
    prop_value = Column(Text)

    __table_args__ = (
        ForeignKeyConstraint(
            ['app_name', 'mdl_name', 'environment'],
            ['java_configs.app_name', 'java_configs.mdl_name', 'java_configs.environment'],
            ondelete="CASCADE"
        ),
    )
    
    config = relationship("ApplicationConfig", back_populates="properties")

class CyberarkConfig(Base):
    """Table 3: Référentiel des identifiants de bases de données (Secrets)."""
    __tablename__ = 'cyberark'
    
    cy_env = Column(String) 
    cy_ref = Column(String, primary_key=True) 
    cy_user = Column(String)
    cy_db_name = Column(String)
    
    db_type = Column(String)
    created_at = Column(Text, default=lambda: datetime.now().strftime("%Y-%m-%d %H:%M:%S"))


# --- Fonctions de Génération de Données (Inchanggées) ---

def generate_db_connections(module_id: str, environment: str) -> Tuple[Dict[str, Any], List[Dict[str, Any]]]:
    """
    Génère un dictionnaire de configurations de DB.
    """
    
    db_configs = {}
    cyberark_entries = []
    
    num_total_db_setups = random.randint(1, 4) 
    cy_env = CYBERARK_ENV_MAP[environment]
    
    selected_mappings = random.sample(ARBITRARY_MAPPINGS, min(num_total_db_setups, len(ARBITRARY_MAPPINGS)))


    for i in range(num_total_db_setups):
        db_type = random.choice(DB_NAMES)
        props = DATABASE_TYPES[db_type]
        
        db_map_id = selected_mappings[i]
        
        # 1. Génération des variables
        cy_ref_num = random.randint(0, 99999) 
        
        # Format C + 5 chiffres
        cy_ref = f"C{cy_ref_num:05d}" 
        
        user_name_base = f"{module_id}_{db_map_id}_user"
        db_name_base = f"{module_id}_{db_map_id}_{db_type}_db"
        
        cy_user = user_name_base.upper() 
        cy_db_name = db_name_base.upper()

        # Enregistrement pour le fichier CSV CyberArk
        cyberark_entries.append({
            'cy_env': cy_env,
            'cy_ref': cy_ref,
            'cy_user': cy_user,
            'cy_db_name': cy_db_name,
            'db_type': db_type
        })
        
        # 2. Construction de l'URL de connexion
        server_host = f"{db_type}-{db_map_id}-{environment}-db.corp.net"
        conn_string = f"{props['conn_prefix']}{server_host}:{random.choice(props['ports'])}/{db_name_base}_{environment}"

        # 3. DÉRIVATION DU NOM DE LA BASE DE DONNÉES PAR MAPPING REGEX
        derived_db_name = get_database_name_from_url(db_type, conn_string)

        # 4. CONSTRUCTION DE LA VALEUR PASSWORD_REFERENCE (Format demandé)
        password_value = (
            f"$({cy_user}_" 
            f"{derived_db_name}_" 
            f"{environment.upper()})" 
        )
        
        # 5. Configuration du module 
        if db_type not in db_configs:
             db_configs[db_type] = {}
             
        db_configs[db_type][db_map_id] = { 
            'driver': props['driver'],
            'url': conn_string,
            'username_reference': cy_user, 
            'password_reference': password_value, 
            'pool_size': random.randint(10, 50),
            'database_name': derived_db_name 
        }
        
    return db_configs, cyberark_entries

def generate_app_module_config(app_id: str, mdl_id: str, environment: str) -> Tuple[Dict[str, Any], List[Dict[str, Any]]]:
    """Génère la config structurée du module et retourne les entrées CyberArk associées."""
    
    prefix = ENVIRONMENT_PREFIXES[environment]
    server_names = [f"{prefix}-{i:02d}-{mdl_id}" for i in random.sample(range(1, 15), random.randint(1, 3))]
    cn_name = f"{mdl_id}.apps.{environment}.corp.com"
    
    # Générer les configs DB et les entrées CyberArk
    db_setups, cyberark_entries = generate_db_connections(mdl_id, environment)
    
    config = {
        'app_name': app_id,
        'mdl_name': mdl_id,
        'environment': environment, 
        
        'version': f"{random.randint(1, 5)}.{random.randint(0, 9)}.{random.randint(0, 9)}",
        'servers': server_names,
        
        'network': {
            'port': random.randint(8080, 9000),
            'timeout_ms': random.choice([500, 1000, 5000]),
        },
        # Structure d'imbrication demandée
        'datasource': {
            'spring': {
                'webcel': {
                    'databases': db_setups, 
                }
            }
        }, 
        'security': {
            'certificate': {
                'cn': cn_name,
                'is_valid': random.choice([True, False]),
                'issuer': 'Corp CA',
            }
        }
    }
    return config, cyberark_entries


# --- Fonctions de Traitement de Fichier et DB (Inchanggées) ---

def flatten_and_format_config(data: dict, app_name: str, mdl_name: str, environment: str, key_prefix: str) -> List[str]:
    """Aplatit un dictionnaire imbriqué en lignes de fichier plat."""
    
    PK_KEYS = ['app_name', 'mdl_name', 'environment'] 
    
    flat_lines = []
    
    def internal_flatten(d: dict, parent_path: str = ''):
        for k, v in d.items():
            if k in PK_KEYS:
                continue

            current_path = parent_path + '.' + k if parent_path else k
            
            if isinstance(v, dict):
                internal_flatten(v, current_path)
            elif isinstance(v, list):
                for idx, item in enumerate(v):
                    if isinstance(item, dict):
                        internal_flatten(item, f"{current_path}.{idx}")
                    else:
                        full_key = f"{key_prefix}.{app_name}.{mdl_name}.{environment}.{current_path}.{idx}"
                        flat_lines.append(f"{full_key}={str(item)}")
            else:
                full_key = f"{key_prefix}.{app_name}.{mdl_name}.{environment}.{current_path}"
                flat_lines.append(f"{full_key}={str(v)}")

    internal_flatten(data)
    return flat_lines


def generate_and_save_files(num_apps: int, num_modules: int, config_file: str, cyberark_file: str):
    """Génère le fichier de configuration plat et le fichier CyberArk CSV."""
    
    total_configs = num_apps * num_modules * len(ENVIRONMENTS)
    print(f"Génération de {total_configs} configurations modules...")
    
    all_flat_lines = []
    all_cyberark_entries = {} 

    for i in range(num_apps):
        app_id = f"app-corp-{i:03d}"
        for j in range(num_modules):
            mdl_id = f"mdl-{random.choice(['auth', 'data', 'api', 'core'])}-{j:01d}"
            
            for env in ENVIRONMENTS:
                # 1. Générer la structure et les entrées CyberArk
                config, cyberark_entries = generate_app_module_config(app_id, mdl_id, env)
                
                # 2. Aplatir et formater la structure pour le fichier plat
                flat_lines = flatten_and_format_config(config, app_id, mdl_id, env, KEY_PREFIX)
                all_flat_lines.extend(flat_lines)
                
                # 3. Collecter les entrées CyberArk
                for entry in cyberark_entries:
                    all_cyberark_entries[entry['cy_ref']] = entry
            
    # --- Écriture du fichier de configuration plat ---
    with open(config_file, 'w') as f:
        f.write('\n'.join(all_flat_lines))
    print(f"✅ Fichier plat '{config_file}' généré avec {len(all_flat_lines)} propriétés.")

    # --- Écriture du fichier CyberArk CSV ---
    cyberark_list = list(all_cyberark_entries.values())
    if not cyberark_list:
        print("⚠️ Aucune entrée CyberArk générée.")
        return 
        
    csv_header = "cy_env,cy_ref,cy_user,cy_db_name\n"
    csv_lines = [csv_header]

    # Génération des lignes CSV 
    for entry in cyberark_list:
        line = f"{entry['cy_env']},{entry['cy_ref']},{entry['cy_user']},{entry['cy_db_name']}\n"
        csv_lines.append(line)
        
    with open(cyberark_file, 'w') as f:
        f.writelines(csv_lines)
        
    print(f"✅ Fichier CyberArk '{cyberark_file}' généré avec {len(cyberark_list)} entrées.")
    
    return len(all_flat_lines), cyberark_list


def reconstruct_configs_from_flat_file(flat_file: str, key_prefix: str) -> List[Dict[str, Any]]:
    
    reconstructed_configs = defaultdict(lambda: {'__temp_structure__': {}})
    
    print(f"\nLecture du fichier plat '{flat_file}' pour la reconstruction...")

    with open(flat_file, 'r') as f:
        for line in f:
            line = line.strip()
            if not line or '=' not in line:
                continue
                
            full_key, value = line.split('=', 1)
            parts = full_key.split('.')
            
            if parts[0] != key_prefix:
                 continue
            
            app_name = parts[1]
            mdl_name = parts[2]
            environment = parts[3]
            prop_path_parts = parts[4:]
            
            config_key = (app_name, mdl_name, environment)
            
            reconstructed_configs[config_key]['app_name'] = app_name
            reconstructed_configs[config_key]['mdl_name'] = mdl_name
            reconstructed_configs[config_key]['environment'] = environment
            
            target = reconstructed_configs[config_key]['__temp_structure__']
            
            for i, part in enumerate(prop_path_parts):
                if i == len(prop_path_parts) - 1:
                    # Conversion de type pour les valeurs
                    try:
                        if value.lower() in ['true', 'false']:
                            val = True if value.lower() == 'true' else False
                        elif value.isdigit():
                            val = int(value)
                        else:
                            val = value
                    except ValueError:
                        val = value
                        
                    target[part] = val
                else:
                    try:
                        # Gérer les listes (indices numériques)
                        index = int(part)
                        if part not in target:
                             target[part] = []
                        # Remplacer les dicts intermédiaires par la liste réelle pour le parcours
                        if isinstance(target[part], dict):
                             target[part] = [] 
                             
                        # Assurer que la liste est assez grande
                        while len(target[part]) <= index:
                            target[part].append({}) 
                            
                        target = target[part][index]
                        
                    except ValueError:
                        # Gérer les dictionnaires (clés de chaîne)
                        if part not in target:
                            target[part] = {}
                        target = target[part]

    final_configs = []
    
    for config_key, data in reconstructed_configs.items():
        # Remplacement de la structure temporaire
        final_config = data['__temp_structure__']
        final_config['app_name'] = data['app_name']
        final_config['mdl_name'] = data['mdl_name']
        final_config['environment'] = data['environment']
        
        final_configs.append(final_config)
        
    print(f"✅ Reconstruction terminée. {len(final_configs)} configurations uniques retrouvées.")
    return final_configs


def load_data_to_postgres(all_configs_list: List[Dict[str, Any]], cyberark_entries: List[Dict[str, Any]], db_url: str):
    
    print("\nPréparation et chargement de la base de données...")
    
    # Utilise DB_URL_TEMPLATE pour la création/nettoyage, en passant "postgres" pour se connecter à la DB par défaut
    ensure_database_and_clean_tables(DB_NAME, DB_URL_TEMPLATE.format("postgres")) 
    
    try:
        # Utilise DB_URL (la chaîne complète) pour la connexion à la DB spécifique
        engine = create_engine(db_url)
             
    except Exception as e:
        print(f"❌ Erreur lors de la création du moteur SQLAlchemy (vérifiez la chaîne de connexion et le pilote): {e}")
        return

    print("   -> Recréation du schéma (tables et contraintes)...")
    Base.metadata.create_all(engine)
    print("   -> Schéma recréé.")
    
    Session = sessionmaker(bind=engine)
    session = Session()

    batch_size = 500
    
    try:
        # 1. Chargement de la table CYBERARK 
        print(f"Insertion des {len(cyberark_entries)} entrées CyberArk...")
        cy_bulk = []
        for entry in cyberark_entries:
            cy_entry = CyberarkConfig(
                cy_env=entry['cy_env'],
                cy_ref=entry['cy_ref'],
                cy_user=entry['cy_user'],
                cy_db_name=entry['cy_db_name'],
                db_type=entry['db_type']
            )
            cy_bulk.append(cy_entry)
        
        session.bulk_save_objects(cy_bulk)
        session.commit()
        print("✅ Chargement de la table 'cyberark' terminé.")
        
        # 2. Chargement des tables JAVA_CONFIGS et APP_PROPERTIES 
        config_bulk = []
        properties_bulk = []
        print(f"Insertion des {len(all_configs_list)} configurations de modules...")

        # Préfixe à ignorer/remplacer dans le chemin des propriétés pour la table app_properties
        DB_PATH_PREFIX = 'datasource.spring.webcel.databases.'

        for config_data in all_configs_list:
            app_name = config_data['app_name']
            mdl_name = config_data['mdl_name']
            environment = config_data['environment']
            
            config_entry = ApplicationConfig(
                app_name=app_name, mdl_name=mdl_name, environment=environment, full_config=config_data
            )
            config_bulk.append(config_entry)
            
            props_for_db = []
            
            def internal_flatten_for_db(d: dict, parent_path: str = ''):
                for k, v in d.items():
                    if k in ['app_name', 'mdl_name', 'environment']: continue
                    
                    current_path = parent_path + '.' + k if parent_path else k
                    
                    if isinstance(v, dict): 
                        # Si nous sommes dans un dictionnaire, continuer la récursion
                        internal_flatten_for_db(v, current_path)
                    elif isinstance(v, list):
                        # Si c'est une liste, itérer sur ses éléments
                        for idx, item in enumerate(v):
                            if isinstance(item, dict): 
                                internal_flatten_for_db(item, f"{current_path}.{idx}")
                            else: 
                                # Gestion des listes de propriétés simples (ex: servers.0)
                                props_for_db.append((f"{current_path}.{idx}", str(item)))
                    else: 
                        # LOGIQUE DE TRANSFORMATION DE LA CLÉ DE BASE DE DONNÉE
                        if current_path.startswith(DB_PATH_PREFIX):
                            
                            db_name_upper = d.get('database_name')
                            
                            if db_name_upper:
                                # Nouveau format: database.database_name_lowercase.property
                                new_key = f"database.{db_name_upper.lower()}.{k}"
                                props_for_db.append((new_key, str(v)))
                                continue 

                        # Si la logique spéciale n'est pas appliquée, utiliser le chemin normal
                        props_for_db.append((current_path, str(v)))


            internal_flatten_for_db(config_data)
            
            for key, value in props_for_db:
                properties_bulk.append(ApplicationProperty(
                    app_name=app_name, mdl_name=mdl_name, environment=environment, prop_key=key, prop_value=value
                ))

            if len(config_bulk) >= batch_size:
                session.bulk_save_objects(config_bulk)
                session.bulk_save_objects(properties_bulk)
                session.commit()
                config_bulk = []; properties_bulk = []

        if config_bulk:
            session.bulk_save_objects(config_bulk)
            session.bulk_save_objects(properties_bulk)
            session.commit()
        
        print(f"✅ Chargement des tables 'java_configs' et 'app_properties' terminé.")

    except Exception as e:
        session.rollback()
        print(f"Une erreur est survenue lors de l'insertion : {e}")
        raise # Rélancer l'erreur pour arrêter le processus si le chargement échoue
    finally:
        session.close()


def ensure_database_and_clean_tables(db_name: str, postgres_default_url: str):
    """Crée la base de données et nettoie les tables existantes. Utilise la chaîne de connexion fournie pour la DB par défaut."""
    print("Connexion au serveur et vérification/nettoyage de la base de données...")
    
    # Tentative de connexion à la base par défaut 'postgres'
    temp_engine = create_engine(postgres_default_url)
    try:
        with temp_engine.connect() as connection:
            connection.execution_options(isolation_level="AUTOCOMMIT").execute(text("COMMIT"))
            
            # Vérifier si la base de données spécifique existe
            db_exists = connection.execute(text(f"SELECT 1 FROM pg_database WHERE datname = '{db_name}'")).fetchone()
            if not db_exists:
                with temp_engine.connect() as conn_create:
                    # Doit utiliser AUTOCOMMIT pour CREATE DATABASE
                    conn_create.execution_options(isolation_level="AUTOCOMMIT").execute(text(f"CREATE DATABASE {db_name}"))
                    
    except OperationalError as e:
        print(f"❌ Erreur de connexion à PostgreSQL. Assurez-vous que le service est démarré et que l'authentification est configurée correctement. Détail: {e}")
        raise
    except Exception as e:
        print(f"Erreur inattendue lors de la vérification/création de la DB: {e}")
        raise
    finally:
        temp_engine.dispose()
        
    # Tentative de suppression des tables dans la base de données spécifique
    # On utilise DB_URL qui est la chaîne complète pour la DB spécifique.
    engine = create_engine(DB_URL) 
    try:
        print("   -> Suppression des tables existantes...")
        Base.metadata.drop_all(engine)
        print("   -> Tables supprimées.")
    except ProgrammingError:
        pass
    except Exception as e:
        print(f"❌ Erreur lors de la suppression des tables : {e}")
        raise
    finally:
        engine.dispose()

# --- Fonctions d'accès à la DB pour l'UI ---

def fetch_module_database_settings(session, app_name: str, mdl_name: str, environment: str) -> Dict[str, Any]:
    """Récupère les configurations de base de données d'un module spécifique."""
    
    config_record = session.query(ApplicationConfig).filter(
        ApplicationConfig.app_name == app_name,
        ApplicationConfig.mdl_name == mdl_name,
        ApplicationConfig.environment == environment
    ).one_or_none()
    
    if not config_record:
        return {"error": f"Configuration non trouvée pour {app_name}.{mdl_name}.{environment}"}

    # Accéder au chemin profond de la configuration DB
    db_configs = config_record.full_config.get('datasource', {}).get('spring', {}).get('webcel', {}).get('databases', {})

    return db_configs

def fetch_app_all_database_settings(session, app_name: str, environment: str) -> Dict[str, Any]:
    """Récupère et consolide TOUTES les configurations de base de données pour tous les modules d'une application."""
    
    module_records = session.query(ApplicationConfig).filter(
        ApplicationConfig.app_name == app_name,
        ApplicationConfig.environment == environment
    ).order_by(ApplicationConfig.mdl_name).all()
    
    consolidated_settings = {}
    
    for record in module_records:
        module_name = record.mdl_name
        
        # Utiliser la même logique que pour un module unique
        db_configs = record.full_config.get('datasource', {}).get('spring', {}).get('webcel', {}).get('databases', {})
        
        # Ajouter les configurations au résultat consolidé, sous la clé du module
        consolidated_settings[module_name] = db_configs
        
    return consolidated_settings

def get_yaml_summary_for_app(session, app_name: str, environment: str) -> Dict[str, Any]:
    """
    Récupère une synthèse des informations de base de données (nom de la DB, nom du password, env) 
    pour tous les modules d'une application donnée.
    """
    
    # Requête consolidée pour récupérer toutes les propriétés de DB nécessaires
    stmt = select(
        ApplicationProperty.mdl_name,
        ApplicationProperty.prop_key,
        ApplicationProperty.prop_value,
        ApplicationProperty.environment
    ).where(
        ApplicationProperty.app_name == app_name,
        ApplicationProperty.environment == environment,
        ApplicationProperty.prop_key.like('database.%') 
    ).order_by(
        ApplicationProperty.mdl_name,
        ApplicationProperty.prop_key
    )

    results = session.execute(stmt).fetchall()
    
    # Structure temporaire pour regrouper les propriétés par module et par DB unique
    # { 'mdl-auth-0': { 'db_id_1': { 'database_name': '...', 'password_reference': '...' }, ... } }
    module_db_map = defaultdict(lambda: defaultdict(dict))
    
    # Collecter les données brutes et les organiser
    for mdl_name, prop_key, prop_value, env in results:
        # Clé dans la table: database.<db_alias>.<property>
        parts = prop_key.split('.')
        if len(parts) < 3:
            continue
            
        db_alias = parts[1] # Ex: mdl-data-0_primary_oracle_db
        property_name = parts[2]       # Ex: database_name ou password_reference

        if property_name in ['database_name', 'password_reference']:
            module_db_map[mdl_name][db_alias][property_name] = prop_value

    
    # Transformer la carte en structure YAML finale en utilisant OrderedDict
    final_modules_list = []
    
    for mdl_name in sorted(module_db_map.keys()):
        db_setups = module_db_map[mdl_name]
        db_list = []
        
        for db_alias, data in sorted(db_setups.items()):
            # S'assurer que les deux clés critiques sont présentes
            if 'database_name' in data and 'password_reference' in data:
                db_list.append(OrderedDict([
                    ('db_alias', db_alias), 
                    ('database_name', data['database_name'].upper()),
                    ('password_reference', data['password_reference']),
                    ('environment', environment.upper())
                ]))
        
        if db_list:
            # Utilisation de OrderedDict pour garantir que 'module_name' est la première clé
            module_dict = OrderedDict([
                ('module_name', mdl_name),
                ('databases', db_list)
            ])
            final_modules_list.append(module_dict)
        
    summary = OrderedDict([
        ('app_name', app_name.upper()),
        ('environment', environment.upper()),
        ('modules', final_modules_list)
    ])

    return summary


# --- CLASSE DE VISUALISATION GRAPHIQUE (TKINTER) ---

class ConfigViewerApp(tk.Tk):
    def __init__(self, db_url: str):
        super().__init__()
        self.title("PostgreSQL Config Viewer - Base de Données")
        self.geometry("1000x700")
        
        self.db_url = db_url 
        
        try:
             self.engine = create_engine(self.db_url) 
             self.Session = sessionmaker(bind=self.engine)
        except Exception as e:
             showerror("Erreur Fatale", f"Impossible de se connecter à la DB avec l'URL: {self.db_url}. Détail: {e}")
             self.destroy()
             return

        self.selected_environment = ENVIRONMENTS[0] 
        
        # Ajout de l'icône (un simple bitmap carré)
        self.app_icon = tk.PhotoImage(width=16, height=16)
        self.app_icon.put("{green}", to=(0, 0, 15, 15))


        # Structure de l'application
        main_pane = ttk.PanedWindow(self, orient=tk.HORIZONTAL)
        main_pane.pack(fill=tk.BOTH, expand=True, padx=5, pady=5)

        # 1. Panneau gauche (Tree View)
        self.tree_frame = ttk.Frame(main_pane, width=300)
        main_pane.add(self.tree_frame)
        self.setup_tree_view()

        # 2. Panneau central (Content View - Notebook)
        self.content_frame = ttk.Frame(main_pane)
        main_pane.add(self.content_frame, weight=1)
        self.setup_content_view()

        self.load_tree_data()

    def setup_tree_view(self):
        self.tree = ttk.Treeview(self.tree_frame, selectmode='browse')
        vsb = ttk.Scrollbar(self.tree_frame, orient="vertical", command=self.tree.yview)
        vsb.pack(side='right', fill='y')
        self.tree.configure(yscrollcommand=vsb.set)
        self.tree.pack(side='left', fill=tk.BOTH, expand=True)

        self.tree.heading("#0", text=f"Applications & Modules ({self.selected_environment.upper()})", anchor='w')
        self.tree.bind('<<TreeviewSelect>>', self.on_tree_select)

        # Configuration des tags de style pour le Treeview
        self.tree.tag_configure("app_node", foreground="#0000FF", font=('Arial', 10, 'bold')) # Bleu
        self.tree.tag_configure("module_node", foreground="#ADD8E6") # Bleu clair

    def setup_content_view(self):
        ttk.Label(self.content_frame, text="Paramètres de Bases de Données", font=("Arial", 12, "bold")).pack(pady=5)
        
        # Création du Notebook (système d'onglets)
        self.notebook = ttk.Notebook(self.content_frame)
        self.notebook.pack(fill=tk.BOTH, expand=True, padx=5, pady=5)

        LIGHT_GREEN_BG = "#E0FFEE"
        
        # --- 1. Onglet JSON (Configuration complète) ---
        self.json_frame = ttk.Frame(self.notebook)
        self.notebook.add(self.json_frame, text="JSON View")
        
        self.json_text = scrolledtext.ScrolledText(
            self.json_frame, 
            wrap=tk.WORD, 
            width=80, 
            height=30, 
            font=("Courier", 10), 
            background=LIGHT_GREEN_BG, 
            foreground="black"
        )
        self.json_text.pack(fill=tk.BOTH, expand=True)
        self.json_text.insert(tk.END, "Sélectionnez un nœud pour afficher le JSON...")

        # --- 2. Onglet YAML (Configuration complète) ---
        self.yaml_frame = ttk.Frame(self.notebook)
        self.notebook.add(self.yaml_frame, text="YAML View")

        self.yaml_text = scrolledtext.ScrolledText(
            self.yaml_frame, 
            wrap=tk.WORD, 
            width=80, 
            height=30, 
            font=("Courier", 10), 
            background=LIGHT_GREEN_BG, 
            foreground="black"
        )
        self.yaml_text.pack(fill=tk.BOTH, expand=True)
        self.yaml_text.insert(tk.END, "Sélectionnez un nœud pour afficher le YAML...")
        
        # --- 3. NOUVEL Onglet YAML (Synthèse Application) ---
        self.summary_frame = ttk.Frame(self.notebook)
        self.notebook.add(self.summary_frame, text="Application Summary (YAML)")

        self.summary_text = scrolledtext.ScrolledText(
            self.summary_frame, 
            wrap=tk.WORD, 
            width=80, 
            height=30, 
            font=("Courier", 10), 
            background=LIGHT_GREEN_BG, 
            foreground="black"
        )
        self.summary_text.pack(fill=tk.BOTH, expand=True)
        self.summary_text.insert(tk.END, "Sélectionnez un nœud d'APPLICATION pour afficher le résumé YAML des bases de données de tous ses modules.")


    def load_tree_data(self):
        session = self.Session()
        try:
            tree_nodes = session.query(ApplicationConfig.app_name, ApplicationConfig.mdl_name).filter(
                ApplicationConfig.environment == self.selected_environment
            ).distinct().order_by(ApplicationConfig.app_name, ApplicationConfig.mdl_name).all()

            self.tree.delete(*self.tree.get_children())
            
            app_map = defaultdict(list)
            for app, mdl in tree_nodes:
                app_map[app].append(mdl)

            # Insertion dans l'arbre
            for app_name in sorted(app_map.keys()):
                app_id = f"APP_{app_name}"
                
                # Ajout de l'icône et du tag de couleur pour l'Application
                self.tree.insert(
                    "", 
                    tk.END, 
                    app_id, 
                    text=app_name.upper(), 
                    tags=("app_node", app_name), # Tag pour la couleur bleue
                    image=self.app_icon, # Icône d'application
                    open=True
                )
                
                for mdl_name in sorted(app_map[app_name]):
                    mdl_id = f"MDL_{app_name}_{mdl_name}"
                    
                    # Ajout du tag de couleur pour le Module
                    self.tree.insert(
                        app_id, 
                        tk.END, 
                        mdl_id, 
                        text=f"• {mdl_name}", 
                        tags=("module_node", app_name, mdl_name) # Tag pour la couleur bleu clair
                    )
            
        except Exception as e:
            showerror("Erreur de Chargement de l'Arbre", f"Impossible de charger l'arbre de navigation. Détail: {e}")
        finally:
            session.close()

    def on_tree_select(self, event):
        selected_items = self.tree.selection()
        if not selected_items:
            return
            
        selected_item = selected_items[0]
        tags = self.tree.item(selected_item, 'tags')
        
        if not tags:
            return

        node_type_tag = tags[0] 
        app_name = tags[1] 
        environment = self.selected_environment
        
        session = self.Session()
        try:
            data_full = {}
            title = ""
            summary_data_ordered = {} # Store the OrderedDict here
            
            if node_type_tag == "app_node":
                # Nœud APPLICATION sélectionné
                title = f"Application: {app_name.upper()} - TOUTES les DB ({environment.upper()})"
                data_full = fetch_app_all_database_settings(session, app_name, environment)
                
                # Fetch the OrderedDict structured for YAML
                summary_data_ordered = get_yaml_summary_for_app(session, app_name, environment) 
                
                # CONVERSION CLÉ: Convertir l'OrderedDict en dict standard pour éviter les erreurs de sérialisation
                # lors de l'affichage de l'exception ou si json.dumps est appelé sur cette structure.
                # data_full doit toujours être un dict standard.
                # L'erreur venait du fait que summary_data_ordered était implicitement utilisé dans un contexte non-YAML.
                # Pour être 100% sûr, le mieux est d'éviter de stocker l'OrderedDict dans une variable qui pourrait être utilisée
                # dans le processus de gestion des erreurs qui fait appel à `json.dumps`.

                # Pour cette démo, on utilise l'OrderedDict directement pour le YAML, et on le garde hors de la variable data_full.
                
            elif node_type_tag == "module_node":
                # Nœud MODULE sélectionné
                mdl_name = tags[2] 
                title = f"Module: {mdl_name} - DB Settings ({environment.upper()})"
                data_full = fetch_module_database_settings(session, app_name, mdl_name, environment)
                summary_data_ordered = {} # Assurer qu'il est vide pour les modules

            # --- 1. Mise à jour des onglets JSON et YAML complets (data_full est un dict standard) ---
            
            json_output = json.dumps(data_full, indent=4)
            self.json_text.delete(1.0, tk.END)
            self.json_text.insert(tk.END, f"--- {title} (JSON) ---\n\n")
            self.json_text.insert(tk.END, json_output)
            
            yaml_output = yaml.safe_dump(data_full, indent=4, default_flow_style=False)
            self.yaml_text.delete(1.0, tk.END)
            self.yaml_text.insert(tk.END, f"--- {title} (YAML) ---\n\n")
            self.yaml_text.insert(tk.END, yaml_output)

            # --- 2. Mise à jour de l'onglet SYNTHÈSE (summary_data_ordered) ---
            self.summary_text.delete(1.0, tk.END)
            if node_type_tag == "app_node" and summary_data_ordered:
                # Utiliser la version OrderedDict ici pour préserver l'ordre pour l'affichage YAML
                yaml_summary_output = yaml.safe_dump(summary_data_ordered, indent=4, default_flow_style=False) 
                self.summary_text.insert(tk.END, f"--- Synthèse de la configuration des DB pour {app_name.upper()} ({environment.upper()}) ---\n\n")
                self.summary_text.insert(tk.END, yaml_summary_output)
            else:
                self.summary_text.insert(tk.END, "Sélectionnez un nœud d'APPLICATION pour afficher la synthèse de tous les modules.")

        except Exception as e:
            # Gestion des erreurs: si l'exception contient un OrderedDict non sérialisable, on le convertit manuellement
            error_details = str(e)
            
            # Vérifier l'argument de l'exception qui pourrait être l'OrderedDict
            if e.args and isinstance(e.args[-1], OrderedDict):
                try:
                    # Convertir en YAML pour l'affichage de l'erreur
                    yaml_representation = yaml.safe_dump(e.args[-1], indent=2, default_flow_style=False)
                    error_details = f"L'objet OrderedDict n'est pas sérialisable par json (voir YAML ci-dessous):\n{yaml_representation}"
                except Exception:
                    # Dernier recours si même yaml.safe_dump échoue
                    error_details = f"Erreur de sérialisation d'objet de données: {e}"
            
            error_message = f"Erreur de récupération/conversion de données:\n\n{error_details}"
            self.json_text.delete(1.0, tk.END)
            self.json_text.insert(tk.END, error_message)
            self.yaml_text.delete(1.0, tk.END)
            self.yaml_text.insert(tk.END, error_message)
            self.summary_text.delete(1.0, tk.END)
            self.summary_text.insert(tk.END, error_message)
        finally:
            session.close()

# --- Point d'entrée du script (inchangé) ---
if __name__ == "__main__":
    
    try:
        # 1. Génération et reconstruction des données
        line_count, cyberark_data = generate_and_save_files(NB_APPLICATIONS, NB_MODULES_PER_APP, OUTPUT_FILENAME, CYBERARK_FILENAME)
        configs_to_load = reconstruct_configs_from_flat_file(OUTPUT_FILENAME, KEY_PREFIX)
        
        # 2. Chargement des données dans PostgreSQL (utilise DB_URL)
        load_data_to_postgres(configs_to_load, cyberark_data, DB_URL)
        
        # 3. LANCEMENT DE L'APPLICATION GRAPHIQUE TKINTER (utilise la MÊME DB_URL)
        print("\n=======================================================")
        print("🚀 Démarrage de l'application graphique Config Viewer...")
        print("=======================================================")
        
        app = ConfigViewerApp(DB_URL)
        app.mainloop()

    except OperationalError:
        print("\n\n###################################################################")
        print("!! ERREUR DE CONNEXION FATALE !!")
        print("Veuillez vérifier les points suivants :")
        print(f"1. Le service PostgreSQL est-il démarré ?")
        print(f"2. Votre configuration PostgreSQL permet-elle une connexion SANS identifiants explicites (par ex. via 'peer' ou 'ident' pour votre utilisateur actuel) ?")
        print(f"URL de connexion tentée : {DB_URL}")
        print("###################################################################\n")
    except Exception as e:
        print(f"\nUne erreur inattendue est survenue : {e}")
