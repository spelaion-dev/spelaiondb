-- The tests' user, in the pluggable database FREEPDB1: user and password `spelaion`. Run by the
-- image on every start (`/opt/oracle/scripts/startup`), so it must work when the user exists.
alter session set container = FREEPDB1;
declare
  existing number;
begin
  select count(*) into existing from dba_users where username = 'SPELAION';
  if existing = 0 then
    -- The lite image has no USERS tablespace: the user's default one, without a quota.
    execute immediate 'create user spelaion identified by spelaion';
    execute immediate 'grant create session, create table, create view, create sequence, create procedure, create materialized view, unlimited tablespace to spelaion';
  end if;
end;
/
