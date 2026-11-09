-- The server groups and servers a real pgAdmin 4 9.19 kept in pgadmin4.db (see README.md).
-- Table definitions exactly as pgAdmin created them; rows as it wrote them, with the
-- home folder in paths replaced. The password columns are empty: nothing was saved.

CREATE TABLE servergroup (
	id INTEGER NOT NULL, 
	user_id INTEGER NOT NULL, 
	name VARCHAR(128) NOT NULL, 
	PRIMARY KEY (id), 
	FOREIGN KEY(user_id) REFERENCES "user" (id) ON DELETE CASCADE, 
	UNIQUE (user_id, name)
);

CREATE TABLE "server" (
	id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, 
	user_id INTEGER NOT NULL, 
	servergroup_id INTEGER NOT NULL, 
	name VARCHAR(128) NOT NULL, 
	host VARCHAR(128), 
	port INTEGER, 
	maintenance_db VARCHAR(64), 
	username VARCHAR(64), 
	comment VARCHAR(1024), 
	password VARCHAR, 
	role VARCHAR(64), 
	discovery_id VARCHAR, 
	db_res VARCHAR, 
	bgcolor VARCHAR(10), 
	fgcolor VARCHAR(10), 
	service VARCHAR, 
	use_ssh_tunnel INTEGER DEFAULT '0', 
	tunnel_host VARCHAR, 
	tunnel_port VARCHAR, 
	tunnel_username VARCHAR, 
	tunnel_authentication INTEGER DEFAULT '0', 
	tunnel_identity_file VARCHAR, 
	tunnel_password VARCHAR, 
	save_password INTEGER DEFAULT '0', 
	shared BOOLEAN, 
	kerberos_conn BOOLEAN DEFAULT 'false', 
	cloud_status INTEGER DEFAULT '0', 
	passexec_cmd VARCHAR, 
	passexec_expiration INTEGER, 
	connection_params JSON, 
	shared_username VARCHAR(64), 
	prepare_threshold INTEGER, 
	tunnel_keep_alive INTEGER DEFAULT '0', 
	tags JSON, 
	is_adhoc INTEGER DEFAULT '0', post_connection_sql VARCHAR, db_res_type VARCHAR(32) DEFAULT 'databases', tunnel_prompt_password INTEGER DEFAULT '0', passexec_name TEXT, 
	FOREIGN KEY(user_id) REFERENCES user (id) ON DELETE CASCADE, 
	FOREIGN KEY(servergroup_id) REFERENCES servergroup (id)
);

INSERT INTO servergroup (id, user_id, name) VALUES (1, 1, 'Servers');
INSERT INTO servergroup (id, user_id, name) VALUES (2, 1, 'Production');
INSERT INTO server (id, user_id, servergroup_id, name, host, port, maintenance_db, username, comment, password, role, discovery_id, db_res, bgcolor, fgcolor, service, use_ssh_tunnel, tunnel_host, tunnel_port, tunnel_username, tunnel_authentication, tunnel_identity_file, tunnel_password, save_password, shared, kerberos_conn, cloud_status, passexec_cmd, passexec_expiration, connection_params, shared_username, prepare_threshold, tunnel_keep_alive, tags, is_adhoc, post_connection_sql, db_res_type, tunnel_prompt_password, passexec_name) VALUES (1, 1, 1, 'Local PG', '127.0.0.1', 15432, 'spelaion', 'spelaion', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, '22', NULL, 0, NULL, NULL, 0, NULL, 0, 0, NULL, NULL, '{"sslmode": "prefer", "connect_timeout": 10}', NULL, NULL, 0, '[]', 0, NULL, 'databases', 0, NULL);
INSERT INTO server (id, user_id, servergroup_id, name, host, port, maintenance_db, username, comment, password, role, discovery_id, db_res, bgcolor, fgcolor, service, use_ssh_tunnel, tunnel_host, tunnel_port, tunnel_username, tunnel_authentication, tunnel_identity_file, tunnel_password, save_password, shared, kerberos_conn, cloud_status, passexec_cmd, passexec_expiration, connection_params, shared_username, prepare_threshold, tunnel_keep_alive, tags, is_adhoc, post_connection_sql, db_res_type, tunnel_prompt_password, passexec_name) VALUES (2, 1, 2, 'Orders TLS', '127.0.0.1', 15432, 'spelaion', 'spelaion', 'Orders of the EU shop', NULL, 'reporting', NULL, NULL, NULL, NULL, NULL, 0, NULL, '22', NULL, 0, NULL, NULL, 0, NULL, 0, 0, NULL, NULL, '{"connect_timeout": 10, "sslrootcert": "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\ca.crt", "sslcert": "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\client.crt", "sslkey": "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\client.key", "sslmode": "verify-ca"}', NULL, NULL, 0, '[{"color": "#3BAA6C", "text": "eu"}]', 0, NULL, 'databases', 0, NULL);
INSERT INTO server (id, user_id, servergroup_id, name, host, port, maintenance_db, username, comment, password, role, discovery_id, db_res, bgcolor, fgcolor, service, use_ssh_tunnel, tunnel_host, tunnel_port, tunnel_username, tunnel_authentication, tunnel_identity_file, tunnel_password, save_password, shared, kerberos_conn, cloud_status, passexec_cmd, passexec_expiration, connection_params, shared_username, prepare_threshold, tunnel_keep_alive, tags, is_adhoc, post_connection_sql, db_res_type, tunnel_prompt_password, passexec_name) VALUES (3, 1, 2, 'PG via bastion', 'postgres', 5432, 'spelaion', 'spelaion', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 1, '127.0.0.1', '2221', 'spelaion', 1, 'C:\Users\tester\spelaiondb\testing\docker\keys\id_ed25519', NULL, 0, NULL, 0, 0, NULL, NULL, '{"sslmode": "prefer", "connect_timeout": 10}', NULL, NULL, 30, '[]', 0, NULL, 'databases', 0, NULL);
INSERT INTO server (id, user_id, servergroup_id, name, host, port, maintenance_db, username, comment, password, role, discovery_id, db_res, bgcolor, fgcolor, service, use_ssh_tunnel, tunnel_host, tunnel_port, tunnel_username, tunnel_authentication, tunnel_identity_file, tunnel_password, save_password, shared, kerberos_conn, cloud_status, passexec_cmd, passexec_expiration, connection_params, shared_username, prepare_threshold, tunnel_keep_alive, tags, is_adhoc, post_connection_sql, db_res_type, tunnel_prompt_password, passexec_name) VALUES (4, 1, 1, 'Analytics service', NULL, 5432, 'postgres', 'postgres', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'analytics', 0, NULL, '22', NULL, 0, NULL, NULL, 0, NULL, 0, 0, NULL, NULL, '{"sslmode": "prefer", "connect_timeout": 10}', NULL, NULL, 0, '[]', 0, NULL, 'databases', 0, NULL);
